//! `login`, `logout`, `push`, `tag` and `search`.

use crate::image::report;
use podbag::image::auth::{self, AuthConfig, Credentials, Stored};
use podbag::image::reference::Reference;
use podbag::image::registry::{self, Registry};
use podbag::{Error, ErrorKind, Image};
use std::io::{BufRead, IsTerminal, Write};

fn credentials() -> Result<Credentials, Error> {
    Credentials::from_env().ok_or_else(|| {
        Error::new(
            ErrorKind::MissingRequirement,
            "cannot find Docker's config directory",
            "neither DOCKER_CONFIG nor the home directory is set",
            "set DOCKER_CONFIG to a directory",
        )
    })
}

fn input_error(what: &str, why: &str, next: &str) -> Error {
    Error::new(ErrorKind::InvalidInput, what, why, next)
}

pub fn login(
    server: Option<&str>,
    username: Option<String>,
    password: Option<String>,
    password_stdin: bool,
) -> i32 {
    match try_login(server, username, password, password_stdin) {
        Ok(()) => 0,
        Err(e) => report(&e),
    }
}

fn try_login(
    server: Option<&str>,
    username: Option<String>,
    password: Option<String>,
    password_stdin: bool,
) -> Result<(), Error> {
    let registry = auth::registry_of(server.unwrap_or("docker.io"));
    let creds = credentials()?;
    if password.is_some() && password_stdin {
        return Err(input_error(
            "cannot log in",
            "--password and --password-stdin both give the password",
            "use one of them",
        ));
    }
    let terminal = std::io::stdin().is_terminal();
    let username = match username {
        Some(u) => u,
        None if password_stdin => {
            return Err(input_error(
                "cannot log in",
                "--password-stdin needs --username",
                "add --username NAME",
            ));
        }
        None if terminal => {
            let known = creds
                .get(&registry)
                .ok()
                .flatten()
                .map(|a| a.username)
                .filter(|u| !u.is_empty());
            let prompt = match &known {
                Some(u) => format!("Username ({u}): "),
                None => "Username: ".to_string(),
            };
            let typed = prompt_line(&prompt, true)?;
            match (typed.is_empty(), known) {
                (true, Some(u)) => u,
                _ => typed,
            }
        }
        None => {
            return Err(input_error(
                "cannot log in",
                "no user name, and no terminal to ask for one",
                "add --username NAME and --password-stdin",
            ));
        }
    };
    if username.is_empty() {
        return Err(input_error(
            "cannot log in",
            "the user name is empty",
            "give a user name",
        ));
    }
    let password = match password {
        Some(p) => {
            eprintln!(
                "WARNING! Using --password on the command line is insecure. Use --password-stdin."
            );
            p
        }
        None if password_stdin => {
            let mut text = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin().lock(), &mut text).map_err(
                |e| {
                    Error::io(
                        "cannot read the password from stdin",
                        &e,
                        "send it on stdin",
                    )
                },
            )?;
            text.trim_end_matches(['\n', '\r']).to_string()
        }
        None if terminal => prompt_line("Password: ", false)?,
        None => {
            return Err(input_error(
                "cannot log in",
                "no password, and no terminal to ask for one",
                "add --password-stdin and send the password on stdin",
            ));
        }
    };
    if password.is_empty() {
        return Err(input_error(
            "cannot log in",
            "the password is empty",
            "give a password",
        ));
    }
    let mut login = AuthConfig::with_password(&username, &password);
    login.serveraddress = auth::key_of(&registry);
    if let Some(token) = Registry::with_auth(Some(login.clone())).login(&registry)? {
        // A refresh token is kept instead of the password, as Docker does.
        login.identitytoken = token;
        login.password.clear();
    }
    if let Stored::File(path) = creds.store(&registry, &login)? {
        eprintln!(
            "WARNING! Your login is stored unencrypted in {}.\n\
             A credential helper (credsStore in that file) keeps it in the system's keychain.",
            path.display()
        );
    }
    println!("Login Succeeded");
    Ok(())
}

/// Ask on the terminal. Without `echo`, what is typed does not show.
fn prompt_line(prompt: &str, echo: bool) -> Result<String, Error> {
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let quiet = if echo { None } else { NoEcho::start() };
    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line);
    if quiet.is_some() {
        eprintln!();
    }
    drop(quiet);
    read.map_err(|e| Error::io("cannot read from the terminal", &e, "try again"))?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

/// The terminal does not show what is typed until this is dropped.
#[cfg(unix)]
struct NoEcho(libc::termios);

#[cfg(unix)]
impl NoEcho {
    fn start() -> Option<NoEcho> {
        // SAFETY: termios calls on stdin with structs we own.
        unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut saved) != 0 {
                return None;
            }
            let mut quiet = saved;
            quiet.c_lflag &= !libc::ECHO;
            libc::tcsetattr(0, libc::TCSANOW, &quiet);
            Some(NoEcho(saved))
        }
    }
}

#[cfg(unix)]
impl Drop for NoEcho {
    fn drop(&mut self) {
        // SAFETY: restores what `start` read.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.0) };
    }
}

/// On Windows the console shows the password: say so, and how to avoid it.
#[cfg(not(unix))]
struct NoEcho;

#[cfg(not(unix))]
impl NoEcho {
    fn start() -> Option<NoEcho> {
        eprint!("(it shows as you type; --password-stdin avoids this) ");
        None
    }
}

pub fn logout(server: Option<&str>) -> i32 {
    let registry = auth::registry_of(server.unwrap_or("docker.io"));
    let key = auth::key_of(&registry);
    match credentials().and_then(|c| c.erase(&registry)) {
        Ok(true) => {
            println!("Removing login credentials for {key}");
            0
        }
        Ok(false) => {
            println!("Not logged in to {key}");
            0
        }
        Err(e) => report(&e),
    }
}

pub fn push(name: &str) -> i32 {
    match Image::push_with_progress(name) {
        Ok(pushed) => {
            let tag = Reference::parse(&pushed.name)
                .ok()
                .and_then(|r| r.tag)
                .unwrap_or_else(|| "latest".to_string());
            println!("{tag}: digest: {} size: {}", pushed.digest, pushed.size);
            0
        }
        Err(e) => report(&e),
    }
}

pub fn tag(source: &str, target: &str) -> i32 {
    match Image::tag(source, target) {
        Ok(_) => 0,
        Err(e) => report(&e),
    }
}

pub fn search(term: &str, limit: u32, no_trunc: bool) -> i32 {
    let found = match registry::search(term, limit) {
        Ok(found) => found,
        Err(e) => return report(&e),
    };
    let rows: Vec<[String; 4]> = found
        .iter()
        .map(|r| {
            let description = r.description.replace(['\n', '\r'], " ");
            let description = if no_trunc || description.chars().count() <= 45 {
                description
            } else {
                format!("{}...", description.chars().take(42).collect::<String>())
            };
            [
                r.name.clone(),
                description,
                r.star_count.to_string(),
                if r.is_official { "[OK]" } else { "" }.to_string(),
            ]
        })
        .collect();
    crate::image::print_table(&["NAME", "DESCRIPTION", "STARS", "OFFICIAL"], &rows);
    0
}
