//! `volume create|ls|rm|inspect`.

use crate::args::VolumeCmd;
use crate::image::report;
use podbag::Volume;

pub fn run(cmd: VolumeCmd) -> i32 {
    match cmd {
        VolumeCmd::Create { labels, name } => match podbag::network::parse_labels(&labels)
            .and_then(|labels| Volume::create_with(name.as_deref(), labels))
        {
            Ok(v) => {
                println!("{}", v.name);
                0
            }
            Err(e) => report(&e),
        },
        VolumeCmd::Ls { quiet } => match Volume::list() {
            Ok(list) => {
                if !quiet {
                    println!("{:<10}VOLUME NAME", "DRIVER");
                }
                for v in list {
                    if quiet {
                        println!("{}", v.name);
                    } else {
                        println!("{:<10}{}", "local", v.name);
                    }
                }
                0
            }
            Err(e) => report(&e),
        },
        VolumeCmd::Rm { force, names } => {
            let mut code = 0;
            for name in names {
                match Volume::remove(&name, force) {
                    Ok(()) => println!("{name}"),
                    Err(e) => code = report(&e),
                }
            }
            code
        }
        VolumeCmd::Inspect { name } => match Volume::get(&name) {
            Ok(v) => {
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                0
            }
            Err(e) => report(&e),
        },
    }
}
