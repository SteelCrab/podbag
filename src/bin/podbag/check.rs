//! `podbag check`: is this system ready for podbag?

use crate::image::report;

pub fn run(apparmor_profile: bool) -> i32 {
    if apparmor_profile {
        let exe = std::env::current_exe().and_then(std::fs::canonicalize);
        return match exe {
            Ok(path) => {
                print!("{}", podbag::envcheck::apparmor_profile(&path));
                0
            }
            Err(e) => report(&podbag::Error::io(
                "cannot find the podbag program",
                &e,
                "run podbag by its full path",
            )),
        };
    }
    check()
}

#[cfg(target_os = "linux")]
fn check() -> i32 {
    use podbag::envcheck::{evaluate, exit_code, gather};
    let items = evaluate(&gather());
    let width = items.iter().map(|i| i.name.len()).max().unwrap_or(0);
    for i in &items {
        println!("{:<5} {:<width$}  {}", i.level.label(), i.name, i.detail);
        if let Some(fix) = &i.fix {
            println!("      {:<width$}  fix: {fix}", "");
        }
    }
    let code = exit_code(&items);
    if code == 0 {
        println!("podbag can run containers here");
    } else {
        println!("podbag cannot run containers here until the failed items are fixed");
    }
    code
}

#[cfg(not(target_os = "linux"))]
fn check() -> i32 {
    report(&podbag::Error::new(
        podbag::ErrorKind::Unsupported,
        "cannot check this system",
        "podbag runs containers only on Linux",
        "run podbag check on Linux",
    ))
}
