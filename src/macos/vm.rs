//! The Linux VM, through Virtualization.framework (objc2-virtualization).
//!
//! A `VZVirtualMachine` belongs to the dispatch queue it was made on: every call to it,
//! and every completion handler it calls, runs there. [`Vm`] keeps the object and sends
//! each call to that queue; the caller waits on a channel.

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::AllocAnyThread;
use objc2::rc::Retained;
use objc2_foundation::{NSArray, NSDictionary, NSError, NSFileHandle, NSString, NSURL};
use objc2_virtualization::*;
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use crate::vm::shares;

/// What the VM is made of.
#[derive(Debug, Clone)]
pub struct VmSpec {
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub disk: PathBuf,
    /// The guest's console goes here.
    pub console: PathBuf,
    pub cpus: usize,
    pub memory: u64,
    pub cmdline: String,
    /// A read-only directory with the guest's path manifest. None keeps the legacy
    /// `users` device for the default configuration.
    pub share_config: Option<PathBuf>,
    pub shared_paths: Vec<PathBuf>,
    /// The network device's address: the same for every VM of a home, so the Mac's DHCP
    /// server gives it its address again.
    pub mac: [u8; 6],
}

/// A started VM.
pub struct Vm {
    queue: DispatchRetained<DispatchQueue>,
    vm: Shared<VZVirtualMachine>,
}

/// An Objective-C object handed between threads. Only the VM's queue touches it; other
/// threads only hold or release it, which is thread-safe.
struct Shared<T: objc2::Message>(Retained<T>);

// SAFETY: see the type's comment: every method call happens on the VM's queue; other
// threads only retain and release, which Objective-C does atomically.
unsafe impl<T: objc2::Message> Send for Shared<T> {}
// SAFETY: as above.
unsafe impl<T: objc2::Message> Sync for Shared<T> {}

impl<T: objc2::Message> Clone for Shared<T> {
    fn clone(&self) -> Self {
        Shared(self.0.clone())
    }
}

fn describe(err: *mut NSError) -> String {
    if err.is_null() {
        return "unknown error".to_string();
    }
    // SAFETY: a non-null NSError the framework gave us for this call.
    unsafe { (*err).localizedDescription().to_string() }
}

fn url(path: &Path) -> Retained<NSURL> {
    NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()))
}

/// The configuration, checked by the framework.
fn configuration(spec: &VmSpec) -> Result<Retained<VZVirtualMachineConfiguration>, String> {
    // SAFETY: plain object creation and property setting on objects we own.
    unsafe {
        let boot =
            VZLinuxBootLoader::initWithKernelURL(VZLinuxBootLoader::alloc(), &url(&spec.kernel));
        boot.setInitialRamdiskURL(Some(&url(&spec.initrd)));
        boot.setCommandLine(&NSString::from_str(&spec.cmdline));

        let config = VZVirtualMachineConfiguration::new();
        config.setBootLoader(Some(&boot));
        config.setPlatform(&VZGenericPlatformConfiguration::new());
        config.setCPUCount(spec.cpus);
        config.setMemorySize(spec.memory);

        // Writes the guest flushes are fsync'd on the Mac (as lima does): they outlive a
        // killed host or VM, as ext4's journal needs; a power cut may lose what the drive's
        // own cache held. F_FULLFSYNC (`Full`) cost 5–7 ms a flush, and creating
        // and removing a container flush several times: `docker run --rm` took 24 ms in
        // the API instead of 4.
        let disk = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_cachingMode_synchronizationMode_error(
            VZDiskImageStorageDeviceAttachment::alloc(),
            &url(&spec.disk),
            false,
            VZDiskImageCachingMode::Cached,
            VZDiskImageSynchronizationMode::Fsync,
        )
        .map_err(|e| format!("the data disk: {}", e.localizedDescription()))?;
        let block = VZVirtioBlockDeviceConfiguration::initWithAttachment(
            VZVirtioBlockDeviceConfiguration::alloc(),
            &disk,
        );
        config.setStorageDevices(&NSArray::from_retained_slice(&[Retained::into_super(
            block,
        )]));

        let net = VZVirtioNetworkDeviceConfiguration::new();
        net.setAttachment(Some(&VZNATNetworkDeviceAttachment::new()));
        let mac = spec.mac.map(|b| format!("{b:02x}")).join(":");
        if let Some(mac) =
            VZMACAddress::initWithString(VZMACAddress::alloc(), &NSString::from_str(&mac))
        {
            net.setMACAddress(&mac);
        }
        config.setNetworkDevices(&NSArray::from_retained_slice(&[Retained::into_super(net)]));

        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&spec.console)
            .map_err(|e| format!("{}: {e}", spec.console.display()))?;
        let handle = NSFileHandle::initWithFileDescriptor_closeOnDealloc(
            NSFileHandle::alloc(),
            log.into_raw_fd(),
            true,
        );
        let serial = VZVirtioConsoleDeviceSerialPortConfiguration::new();
        serial.setAttachment(Some(
            &VZFileHandleSerialPortAttachment::initWithFileHandleForReading_fileHandleForWriting(
                VZFileHandleSerialPortAttachment::alloc(),
                None,
                Some(&handle),
            ),
        ));
        config.setSerialPorts(&NSArray::from_retained_slice(&[Retained::into_super(
            serial,
        )]));

        config.setSocketDevices(&NSArray::from_retained_slice(&[Retained::into_super(
            VZVirtioSocketDeviceConfiguration::new(),
        )]));
        config.setEntropyDevices(&NSArray::from_retained_slice(&[Retained::into_super(
            VZVirtioEntropyDeviceConfiguration::new(),
        )]));
        config.setMemoryBalloonDevices(&NSArray::from_retained_slice(&[Retained::into_super(
            VZVirtioTraditionalMemoryBalloonDeviceConfiguration::new(),
        )]));

        let (tag, share): (_, Retained<VZDirectoryShare>) = match &spec.share_config {
            Some(manifest) => {
                let mut names = vec![NSString::from_str(shares::CONFIG)];
                let mut dirs = vec![VZSharedDirectory::initWithURL_readOnly(
                    VZSharedDirectory::alloc(),
                    &url(manifest),
                    true,
                )];
                for (index, path) in spec.shared_paths.iter().enumerate() {
                    names.push(NSString::from_str(&shares::entry(index)));
                    dirs.push(VZSharedDirectory::initWithURL_readOnly(
                        VZSharedDirectory::alloc(),
                        &url(path),
                        false,
                    ));
                }
                let names: Vec<&NSString> = names.iter().map(|name| &**name).collect();
                let dirs = NSDictionary::from_retained_objects(&names, &dirs);
                let share = VZMultipleDirectoryShare::initWithDirectories(
                    VZMultipleDirectoryShare::alloc(),
                    &dirs,
                );
                (shares::TAG, Retained::into_super(share))
            }
            None => {
                let dir = VZSharedDirectory::initWithURL_readOnly(
                    VZSharedDirectory::alloc(),
                    &url(Path::new(shares::DEFAULT)),
                    false,
                );
                let share = VZSingleDirectoryShare::initWithDirectory(
                    VZSingleDirectoryShare::alloc(),
                    &dir,
                );
                ("users", Retained::into_super(share))
            }
        };
        let fs = VZVirtioFileSystemDeviceConfiguration::initWithTag(
            VZVirtioFileSystemDeviceConfiguration::alloc(),
            &NSString::from_str(tag),
        );
        fs.setShare(Some(&share));
        config
            .setDirectorySharingDevices(&NSArray::from_retained_slice(&[Retained::into_super(fs)]));

        config
            .validateWithError()
            .map_err(|e| e.localizedDescription().to_string())?;
        Ok(config)
    }
}

impl Vm {
    /// Make the VM and start it; returns once the framework says it runs.
    pub fn start(spec: &VmSpec) -> Result<Vm, String> {
        let config = Shared(configuration(spec)?);
        let queue = DispatchQueue::new("dev.podbag.vm", None);
        let (made, take) = mpsc::channel();
        let on_queue = queue.clone();
        queue.exec_sync(move || {
            // The whole wrapper, not its field: closures capture fields separately.
            let config = config;
            // SAFETY: made on the queue it belongs to.
            let vm = unsafe {
                VZVirtualMachine::initWithConfiguration_queue(
                    VZVirtualMachine::alloc(),
                    &config.0,
                    &on_queue,
                )
            };
            let _ = made.send(Shared(vm));
        });
        let vm = take.recv().map_err(|_| "the VM was not made".to_string())?;
        let (done, wait) = mpsc::channel();
        let started = vm.clone();
        queue.exec_async(move || {
            let started = started;
            let block = RcBlock::new(move |err: *mut NSError| {
                let _ = done.send(if err.is_null() {
                    Ok(())
                } else {
                    Err(describe(err))
                });
            });
            // SAFETY: on the VM's queue.
            unsafe { started.0.startWithCompletionHandler(&block) };
        });
        wait.recv_timeout(Duration::from_secs(30))
            .map_err(|_| "the VM did not start within 30 seconds".to_string())??;
        Ok(Vm { queue, vm })
    }

    /// Whether the VM still runs (or is starting). When its queue does not answer in time,
    /// it is taken to run: the host must not end (and so kill it) on a guess.
    pub fn running(&self) -> bool {
        let (tx, rx) = mpsc::channel();
        let vm = self.vm.clone();
        self.queue.exec_async(move || {
            let vm = vm;
            // SAFETY: on the VM's queue.
            let state = unsafe { vm.0.state() };
            let _ = tx.send(
                state == VZVirtualMachineState::Running || state == VZVirtualMachineState::Starting,
            );
        });
        rx.recv_timeout(Duration::from_secs(5)).unwrap_or(true)
    }

    /// A connection to the guest's vsock `port`.
    pub fn connect(&self, port: u32) -> std::io::Result<OwnedFd> {
        let (tx, rx) = mpsc::channel::<std::io::Result<OwnedFd>>();
        let vm = self.vm.clone();
        self.queue.exec_async(move || {
            let vm = vm;
            // SAFETY: on the VM's queue; the socket device is the one we configured.
            unsafe {
                let device = vm.0.socketDevices().firstObject();
                let Some(device) = device.and_then(|d| d.downcast::<VZVirtioSocketDevice>().ok())
                else {
                    let _ = tx.send(Err(std::io::Error::other("the VM has no vsock device")));
                    return;
                };
                let block = RcBlock::new(
                    move |conn: *mut VZVirtioSocketConnection, err: *mut NSError| {
                        if conn.is_null() {
                            let _ = tx.send(Err(std::io::Error::new(
                                std::io::ErrorKind::ConnectionRefused,
                                describe(err),
                            )));
                            return;
                        }
                        // Our own descriptor: the connection object closes its own.
                        let fd = libc::dup((*conn).fileDescriptor());
                        (*conn).close();
                        let _ = tx.send(if fd < 0 {
                            Err(std::io::Error::last_os_error())
                        } else {
                            Ok(OwnedFd::from_raw_fd(fd))
                        });
                    },
                );
                device.connectToPort_completionHandler(port, &block);
            }
        });
        rx.recv_timeout(Duration::from_secs(10)).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::TimedOut, "vsock connect timed out")
        })?
    }

    /// Stop at once (the guest powers off by itself first, normally).
    pub fn stop(&self) {
        let (tx, rx) = mpsc::channel();
        let vm = self.vm.clone();
        self.queue.exec_async(move || {
            let vm = vm;
            let block = RcBlock::new(move |_err: *mut NSError| {
                let _ = tx.send(());
            });
            // SAFETY: on the VM's queue.
            unsafe { vm.0.stopWithCompletionHandler(&block) };
        });
        let _ = rx.recv_timeout(Duration::from_secs(10));
    }
}
