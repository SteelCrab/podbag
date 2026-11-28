//! `build`, `commit`, `export` and `diff`.

use crate::args::BuildArgs;
#[cfg(target_os = "linux")]
use crate::buildargs::pairs;
use crate::image::report;
use podbag::{ContainerHandle, Error, ErrorKind};

pub fn build(a: BuildArgs) -> i32 {
    #[cfg(target_os = "linux")]
    {
        match run_build(a) {
            Ok(()) => 0,
            Err(e) => report(&e),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = a;
        crate::unsupported()
    }
}

#[cfg(target_os = "linux")]
fn run_build(a: BuildArgs) -> podbag::Result<()> {
    use podbag::build::engine::{self, BuildOptions};
    use std::io::{Read, Write};
    use std::path::PathBuf;
    let mut unpacked = None;
    let mut stdin_dockerfile = None;
    let context: PathBuf = if a.context == "-" {
        let mut bytes = Vec::new();
        std::io::stdin().read_to_end(&mut bytes).map_err(|e| {
            Error::io(
                "cannot read the build context from stdin",
                &e,
                "send a tar archive",
            )
        })?;
        let packing = podbag::build::plan::packing(&bytes[..bytes.len().min(512)]);
        let tar = if packing == podbag::build::plan::Packing::None {
            // Not an archive: a Dockerfile, with an empty context.
            stdin_dockerfile = Some(String::from_utf8_lossy(&bytes).into_owned());
            vec![0u8; 1024]
        } else {
            bytes
        };
        let ctx = engine::unpack_context(&mut &tar[..])?;
        let dir = ctx.dir.clone();
        unpacked = Some(ctx);
        dir
    } else {
        std::fs::canonicalize(&a.context)
            .ok()
            .filter(|p| p.is_dir())
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::NotFound,
                    format!("cannot use {} as the build context", a.context),
                    "it is not a directory",
                    "give a directory, or - for a tar archive on stdin",
                )
            })?
    };
    let (dockerfile, name, ignore) = match (&stdin_dockerfile, &a.file) {
        (Some(text), _) => (text.clone(), "Dockerfile".to_string(), String::new()),
        (None, Some(f)) => {
            let path = if unpacked.is_some() {
                context.join(f)
            } else {
                PathBuf::from(f)
            };
            let text = std::fs::read_to_string(&path).map_err(|e| {
                Error::io(
                    format!("cannot read the Dockerfile {f}"),
                    &e,
                    "check the -f path",
                )
            })?;
            (text, f.clone(), engine::read_ignore(&context, &path))
        }
        (None, None) => {
            let (text, ignore) = engine::read_dockerfile(&context, "Dockerfile")?;
            (text, "Dockerfile".to_string(), ignore)
        }
    };
    let opts = BuildOptions {
        context,
        dockerfile,
        dockerfile_name: name,
        ignore,
        tags: a.tag,
        build_args: pairs(&a.build_arg, true),
        target: a.target,
        no_cache: a.no_cache,
        pull: a.pull,
        labels: pairs(&a.label, false),
        network: a.network,
        quiet: a.quiet,
        // Logins come from the front end or Docker's config, as for `pull`.
        registry_logins: Vec::new(),
    };
    let mut stdout = std::io::stdout();
    let result = engine::build(&opts, &mut |text| {
        let _ = stdout.write_all(text.as_bytes());
        let _ = stdout.flush();
    });
    drop(unpacked);
    result.map(drop)
}

pub fn commit(
    container: &str,
    image: Option<String>,
    changes: Vec<String>,
    author: &str,
    message: &str,
) -> i32 {
    let made = ContainerHandle::get(container).and_then(|c| {
        c.commit_with_metadata(
            &podbag::build::CommitOptions {
                name: image,
                changes,
            },
            author,
            message,
        )
    });
    match made {
        Ok(built) => {
            println!("{}", built.id);
            0
        }
        Err(e) => report(&e),
    }
}

pub fn export(container: &str, output: Option<&std::path::Path>) -> i32 {
    use std::io::IsTerminal;
    let exported = (|| -> podbag::Result<()> {
        let c = ContainerHandle::get(container)?;
        let mut out: Box<dyn std::io::Write> = match output {
            Some(path) => Box::new(std::fs::File::create(path).map_err(|e| {
                Error::io(
                    format!("cannot create {}", path.display()),
                    &e,
                    "check the path",
                )
            })?),
            None if std::io::stdout().is_terminal() => {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "cannot export to a terminal",
                    "the archive is binary",
                    "redirect stdout to a file, or use -o FILE",
                ));
            }
            None => Box::new(std::io::stdout().lock()),
        };
        let mut tar = c.export()?;
        std::io::copy(&mut tar, &mut out)
            .and_then(|_| out.flush())
            .map_err(|e| Error::io("cannot export the container", &e, "try again"))?;
        Ok(())
    })();
    match exported {
        Ok(()) => 0,
        Err(e) => report(&e),
    }
}

pub fn diff(container: &str) -> i32 {
    match ContainerHandle::get(container).and_then(|c| c.changes()) {
        Ok(list) => {
            for c in list {
                let kind = match c.kind {
                    1 => 'A',
                    2 => 'D',
                    _ => 'C',
                };
                println!("{kind} {}", c.path);
            }
            0
        }
        Err(e) => report(&e),
    }
}
