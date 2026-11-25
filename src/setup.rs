//! `r3akt-sigil setup`: a guided first installation. It installs the speech
//! model, tests the RTL-SDR and writes the gateway settings.

use std::fmt::Display;
use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use toml_edit::{Array, DocumentMut, value};

use crate::config::{self, Config};
use crate::correlation::radio_id;
use crate::identity::{PASSPHRASE_ENV, write_atomic};
use crate::model::{Release, State};
use crate::output::frequency_label;
use crate::radio;

/// A new settings file starts from the example, so it keeps its comments.
const EXAMPLE: &str = include_str!("../gateway.example.toml");

#[cfg(windows)]
const DRIVER_HINT: &str = "On Windows the dongle's SDR interface must use the WinUSB driver; README.md links the installation guide.";
#[cfg(not(windows))]
const DRIVER_HINT: &str =
    "On Linux the account needs USB access to the dongle; README.md gives the udev rule.";

pub struct Setup<'a> {
    /// Gateway settings file to write.
    pub config: &'a Path,
    pub release: &'a Release,
    /// Where the speech model is, or goes.
    pub model: &'a Path,
    /// Receive frequency to suggest.
    pub frequency_hz: u32,
    /// How this executable was started, with the global options `listen`
    /// needs again.
    pub program: &'a str,
}

struct Terminal<'a> {
    input: &'a mut dyn BufRead,
    output: &'a mut dyn Write,
}

impl Terminal<'_> {
    fn say(&mut self, text: impl Display) -> Result<()> {
        writeln!(self.output, "{text}")?;
        Ok(())
    }

    /// Asks until `parse` accepts the answer. An empty answer means `default`.
    fn ask<T>(
        &mut self,
        question: &str,
        default: &str,
        parse: impl Fn(&str) -> Result<T>,
    ) -> Result<T> {
        loop {
            if default.is_empty() {
                write!(self.output, "{question}: ")?;
            } else {
                write!(self.output, "{question} [{default}]: ")?;
            }
            self.output.flush()?;
            let mut line = String::new();
            ensure!(
                self.input.read_line(&mut line)? > 0,
                "setup ended before it was finished; the gateway settings were not written"
            );
            let answer = line.trim();
            match parse(if answer.is_empty() { default } else { answer }) {
                Ok(value) => return Ok(value),
                Err(error) => writeln!(self.output, "  {error:#}")?,
            }
        }
    }

    fn confirm(&mut self, question: &str, default: bool) -> Result<bool> {
        self.ask(
            question,
            if default { "yes" } else { "no" },
            |answer| match answer.to_ascii_lowercase().as_str() {
                "y" | "yes" => Ok(true),
                "n" | "no" => Ok(false),
                _ => bail!("answer yes or no"),
            },
        )
    }
}

/// The hub settings the wizard asks for.
struct Hub {
    destination_hash: String,
    tcp_clients: Vec<String>,
    send_intents: bool,
    unidentified_sender: bool,
}

pub fn run(setup: &Setup, input: &mut dyn BufRead, output: &mut dyn Write) -> Result<()> {
    let terminal = &mut Terminal { input, output };
    let existing = setup
        .config
        .exists()
        .then(|| Config::load(setup.config))
        .transpose()
        .with_context(|| {
            format!(
                "{} is not valid; fix or move it, then run setup again",
                setup.config.display()
            )
        })?;

    terminal.say("R3AKT Sigil setup")?;
    terminal.say("Press Enter to accept the answer in [brackets]. Ctrl+C stops setup.")?;
    if existing.is_some() {
        terminal.say(format_args!(
            "Your answers update the existing {}; its other settings stay as they are.",
            setup.config.display()
        ))?;
    }

    terminal.say("\nStep 1 of 4: speech model")?;
    let model_ready = model(setup, terminal)?;

    terminal.say("\nStep 2 of 4: receiver")?;
    let (frequency_hz, gain_db) = receiver(setup, terminal)?;

    terminal.say("\nStep 3 of 4: hub and radios")?;
    let fresh = Config::default();
    let defaults = existing.as_ref().unwrap_or(&fresh);
    let hub = hub(terminal, defaults, existing.is_some())?;
    let known: Vec<&str> = defaults
        .identity
        .allowlist
        .iter()
        .map(String::as_str)
        .collect();
    let allowlist = terminal.ask(
        "BSS IDs of your radios, as the radios send them, separated by commas",
        &known.join(", "),
        |answer| {
            let radios = list(answer, |id| {
                radio_id(id).with_context(|| {
                    format!("{id:?} is not a BSS ID: use up to 32 letters, digits, - or _")
                })
            })?;
            ensure!(
                hub.is_none() || !radios.is_empty(),
                "name at least one radio; only listed radios are published"
            );
            Ok(radios)
        },
    )?;

    terminal.say("\nStep 4 of 4: write the settings")?;
    let base = if existing.is_some() {
        std::fs::read_to_string(setup.config)
            .with_context(|| format!("read {}", setup.config.display()))?
    } else {
        EXAMPLE.to_owned()
    };
    let settings = settings(&base, hub.as_ref(), &allowlist)?;
    let directory = match setup.config.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    Config::parse(&settings, directory).context("the new settings are not valid")?;
    let question = if existing.is_some() {
        format!("Replace {}?", setup.config.display())
    } else {
        format!("Write {}?", setup.config.display())
    };
    if !terminal.confirm(&question, true)? {
        return terminal.say("No settings were written.");
    }
    std::fs::create_dir_all(directory)
        .with_context(|| format!("create {}", directory.display()))?;
    let file = setup
        .config
        .file_name()
        .context("the settings path names no file")?;
    write_atomic(&directory.join(file), settings.as_bytes(), true)
        .with_context(|| format!("write {}", setup.config.display()))?;
    terminal.say(format_args!("Wrote {}.", setup.config.display()))?;

    next_steps(
        setup,
        terminal,
        model_ready,
        hub.is_some(),
        frequency_hz,
        gain_db,
    )
}

/// Asks for the frequency and gain and offers to test the dongle with them.
fn receiver(setup: &Setup, terminal: &mut Terminal) -> Result<(u32, f32)> {
    let frequency_hz = terminal.ask(
        "Receive frequency in MHz",
        &(f64::from(setup.frequency_hz) / 1e6).to_string(),
        frequency,
    )?;
    let (gain_db, receiver) = terminal.ask(
        "Tuner gain in dB",
        &radio::Settings::DEFAULT_GAIN_DB.to_string(),
        |answer| {
            let gain_db: f32 = answer.parse().context("enter a gain in dB, such as 16.6")?;
            let receiver = radio::Settings::at_gain(gain_db);
            receiver.validate()?;
            Ok((gain_db, receiver))
        },
    )?;
    if terminal.confirm(
        "Test the RTL-SDR now? Plug it in and stop any running gateway first",
        true,
    )? {
        match radio::open(frequency_hz, &receiver) {
            Ok((_device, gain)) => terminal.say(format_args!(
                "RTL-SDR found and tuned to {}, gain {gain}.",
                frequency_label(frequency_hz)
            ))?,
            Err(error) => {
                terminal.say(format_args!("The RTL-SDR did not open: {error:#}"))?;
                terminal.say(DRIVER_HINT)?;
                terminal.say("Setup goes on; the gateway will need the dongle when it starts.")?;
            }
        }
    }
    Ok((frequency_hz, gain_db))
}

/// Finds or installs the speech model. Returns whether one is in place.
fn model(setup: &Setup, terminal: &mut Terminal) -> Result<bool> {
    let Setup { release, model, .. } = setup;
    terminal.say(format_args!("Checking {}", model.display()))?;
    match release.check(model)? {
        State::Verified => {
            terminal.say(format_args!(
                "Found {}; its files are intact.",
                release.name
            ))?;
            return Ok(true);
        }
        State::Different(what) => {
            terminal.say(format_args!(
                "This is not the {} release this build expects: {what}.",
                release.name
            ))?;
            terminal.say(
                "Setup leaves it alone and the gateway will try to load it. To replace it, delete it and run setup again.",
            )?;
            return Ok(true);
        }
        State::Missing => {}
    }
    terminal.say(format_args!(
        "Sigil recognizes speech offline with {} (licence {}). It is not installed.",
        release.name, release.licence
    ))?;
    if terminal.confirm(
        &format!(
            "Download it now ({} MB) from {}?",
            release.megabytes(),
            release.url
        ),
        true,
    )? {
        let mut tenths = 0;
        let fetched = release.fetch(model, &mut |done, total| {
            let reached = done * 10 / total.max(1);
            if reached > tenths {
                tenths = reached;
                // Progress is a courtesy; a closed terminal must not stop the download.
                let _ = writeln!(
                    terminal.output,
                    "  {} of {} MB",
                    done / 1_000_000,
                    total / 1_000_000
                );
            }
        });
        match fetched {
            Ok(()) => {
                terminal.say(format_args!(
                    "Installed and verified in {}.",
                    model.display()
                ))?;
                return Ok(true);
            }
            Err(error) => terminal.say(format_args!("The download failed: {error:#}"))?,
        }
    }
    terminal.say(format_args!(
        "To install it by hand, download {} and put the model in {}.",
        release.url,
        model.display()
    ))?;
    Ok(false)
}

fn hub(terminal: &mut Terminal, defaults: &Config, updating: bool) -> Result<Option<Hub>> {
    if !terminal.confirm(
        "Publish to an RCH hub over Reticulum?",
        defaults.rch.enabled,
    )? {
        terminal.say("The gateway will write its events to local files only.")?;
        return Ok(None);
    }
    let destination_hash = terminal.ask(
        "LXMF delivery hash of the hub (32 hexadecimal characters)",
        &defaults.rch.destination_hash,
        |answer| {
            ensure!(
                config::is_destination_hash(answer),
                "enter the hub's 32 hexadecimal characters"
            );
            Ok(answer.to_ascii_lowercase())
        },
    )?;
    let tcp_clients = terminal.ask(
        "Reticulum TCP interfaces as host:port, separated by commas",
        &defaults.fabric.tcp_clients.join(", "),
        |answer| {
            let endpoints = list(answer, |endpoint| {
                config::check_endpoint(endpoint)
                    .with_context(|| format!("{endpoint:?}"))
                    .map(|()| endpoint.to_owned())
            })?;
            ensure!(!endpoints.is_empty(), "enter at least one host:port");
            Ok(endpoints)
        },
    )?;
    terminal.say(
        "A radio ID is not authenticated: anyone who transmits a listed ID is published as that radio.",
    )?;
    let send_intents = terminal.confirm(
        "Also act on Kodiak voice commands (reports, markers and hub commands)?",
        updating && defaults.rch.send_intents,
    )?;
    let unidentified_sender = terminal.confirm(
        "Publish speech that carried no radio ID, from a separate gateway identity?",
        defaults.identity.unidentified_sender,
    )?;
    Ok(Some(Hub {
        destination_hash,
        tcp_clients,
        send_intents,
        unidentified_sender,
    }))
}

/// `base` with the answers written into it. Everything else, comments
/// included, stays as it is.
fn settings(base: &str, hub: Option<&Hub>, allowlist: &[String]) -> Result<String> {
    let mut document: DocumentMut = base.parse().context("read the settings to update")?;
    let mut set = |section: &str, key: &str, item: toml_edit::Item| {
        document.entry(section).or_insert_with(toml_edit::table)[key] = item;
    };
    set("identity", "allowlist", value(Array::from_iter(allowlist)));
    set("rch", "enabled", value(hub.is_some()));
    if let Some(hub) = hub {
        set("rch", "destination_hash", value(&hub.destination_hash));
        set("rch", "send_intents", value(hub.send_intents));
        set(
            "fabric",
            "tcp_clients",
            value(Array::from_iter(&hub.tcp_clients)),
        );
        set(
            "identity",
            "unidentified_sender",
            value(hub.unidentified_sender),
        );
    }
    Ok(document.to_string())
}

fn next_steps(
    setup: &Setup,
    terminal: &mut Terminal,
    model_ready: bool,
    publishing: bool,
    frequency_hz: u32,
    gain_db: f32,
) -> Result<()> {
    terminal.say("\nNext steps")?;
    if !model_ready {
        terminal
            .say("- Install the speech model: run setup again, or follow the note in step 1.")?;
    }
    if publishing {
        terminal.say(format_args!(
            "- In the terminal that will run the gateway, set {PASSPHRASE_ENV} to the passphrase that encrypts the radio identities. Use at least 16 characters and keep it apart from your backups:"
        ))?;
        if cfg!(windows) {
            terminal
                .say("    $secure = Read-Host 'Identity registry passphrase' -AsSecureString")?;
            terminal.say(format_args!(
                "    $env:{PASSPHRASE_ENV} = [System.Net.NetworkCredential]::new('', $secure).Password"
            ))?;
        } else {
            terminal.say(format_args!(
                "    read -rs {PASSPHRASE_ENV} && export {PASSPHRASE_ENV}"
            ))?;
        }
    }
    terminal
        .say("- Start the gateway and leave the channel idle until it prints \"Listening on\":")?;
    terminal.say(format_args!(
        "    {} --frequency-hz {frequency_hz} listen --config {} --gain-db {gain_db}",
        setup.program,
        quoted(&setup.config.display().to_string())
    ))
}

/// `text` as one shell argument.
pub fn quoted(text: &str) -> String {
    if text.contains(char::is_whitespace) {
        format!("\"{text}\"")
    } else {
        text.to_owned()
    }
}

fn frequency(answer: &str) -> Result<u32> {
    let hz = (answer
        .parse::<f64>()
        .context("enter a frequency in MHz, such as 435.995")?
        * 1e6)
        .round();
    ensure!(
        (1.0..=f64::from(u32::MAX)).contains(&hz),
        "enter a frequency in MHz, such as 435.995"
    );
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "checked to be a whole number within u32"
    )]
    Ok(hz as u32)
}

/// The comma-separated items of `answer`, each read by `item`.
fn list<T>(answer: &str, item: impl Fn(&str) -> Result<T>) -> Result<Vec<T>> {
    answer
        .split(',')
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(item)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::WHISPER_BASE_EN;

    const HUB: &str = "0123456789abcdef0123456789ABCDEF";

    /// Runs the wizard in `root` with `answers`, one per line. The model is
    /// absent there, so the first answer is to the download question.
    fn wizard(root: &Path, answers: &str) -> (Result<()>, String) {
        let config = root.join("gateway.toml");
        let model = root.join("models/absent.bin");
        let setup = Setup {
            config: &config,
            release: &WHISPER_BASE_EN,
            model: &model,
            frequency_hz: 435_995_000,
            program: "sigil",
        };
        let mut output = Vec::new();
        let result = run(&setup, &mut answers.as_bytes(), &mut output);
        (result, String::from_utf8(output).unwrap())
    }

    #[test]
    fn a_first_setup_writes_settings_the_gateway_accepts() {
        let root = tempfile::tempdir().unwrap();
        let answers =
            format!("no\n\n16.6\nno\nyes\n{HUB}\nrmap.world:4242, 10.0.0.5:4242\n\n\nr4, R5\n\n");
        let (result, output) = wizard(root.path(), &answers);
        result.unwrap();

        let path = root.path().join("gateway.toml");
        let config = Config::load(&path).unwrap();
        assert!(config.rch.enabled);
        assert_eq!(config.rch.destination_hash, HUB.to_ascii_lowercase());
        assert!(!config.rch.send_intents, "commands are off unless chosen");
        assert!(config.rch.send_chat && config.rch.send_telemetry);
        assert_eq!(
            config.fabric.tcp_clients,
            ["rmap.world:4242", "10.0.0.5:4242"]
        );
        assert!(config.identity.allowlist.iter().eq(["R4", "R5"]));
        assert!(config.identity.auto_register);
        assert!(!config.identity.unidentified_sender);
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("# A VR-N76 may send its BSS packet"));

        assert!(
            output.contains("sigil --frequency-hz 435995000 listen --config ")
                && output.contains(" --gain-db 16.6"),
            "{output}"
        );
        assert!(output.contains(PASSPHRASE_ENV));
        assert!(output.contains("Install the speech model"));
    }

    #[test]
    fn a_local_only_setup_needs_no_hub_or_radios() {
        let root = tempfile::tempdir().unwrap();
        let (result, output) = wizard(root.path(), "no\n145.5\n\nno\nno\n\nyes\n");
        result.unwrap();

        let config = Config::load(&root.path().join("gateway.toml")).unwrap();
        assert!(!config.rch.enabled);
        assert!(config.identity.allowlist.is_empty());
        assert!(output.contains("--frequency-hz 145500000 "), "{output}");
        assert!(output.contains("--gain-db 29.7"), "{output}");
        assert!(!output.contains(PASSPHRASE_ENV));
    }

    #[test]
    fn an_answer_that_is_not_valid_is_asked_again() {
        let root = tempfile::tempdir().unwrap();
        let answers = format!(
            "no\nfour\n435.995\n99\n16.6\nmaybe\nno\nyes\nnot-a-hash\n{HUB}\nrmap.world\nrmap.world:4242\n\n\n\nR 4\nR4\nyes\n"
        );
        let (result, output) = wizard(root.path(), &answers);
        result.unwrap();

        for complaint in [
            "enter a frequency in MHz",
            "--gain-db must be between -10 and 60 dB",
            "answer yes or no",
            "enter the hub's 32 hexadecimal characters",
            "Fabric endpoint must be host:port",
            "name at least one radio",
            "\"R 4\" is not a BSS ID",
        ] {
            assert!(output.contains(complaint), "no {complaint:?} in {output}");
        }
        let config = Config::load(&root.path().join("gateway.toml")).unwrap();
        assert!(config.identity.allowlist.iter().eq(["R4"]));
    }

    #[test]
    fn a_second_setup_keeps_what_it_does_not_ask_about() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("gateway.toml");
        std::fs::write(
            &path,
            format!(
                "# Site: north relay\n[identity]\nallowlist = [\"R4\"]\n\n[fabric]\ntcp_clients = [\"rmap.world:4242\"]\n\n[rch]\nenabled = true\ndestination_hash = \"{HUB}\"\nchat_header = true\nsend_intents = true\n"
            ),
        )
        .unwrap();

        // Every hub answer is left at what the file already says.
        let (result, output) = wizard(root.path(), "no\n\n\nno\n\n\n\n\n\nR4, R7\n\n");
        result.unwrap();

        assert!(output.contains("Replace "), "{output}");
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.starts_with("# Site: north relay\n"), "{written}");
        let config = Config::load(&path).unwrap();
        assert!(config.rch.enabled && config.rch.chat_header && config.rch.send_intents);
        assert_eq!(config.rch.destination_hash, HUB.to_ascii_lowercase());
        assert_eq!(config.fabric.tcp_clients, ["rmap.world:4242"]);
        assert!(config.identity.allowlist.iter().eq(["R4", "R7"]));
    }

    #[test]
    fn nothing_is_written_without_the_final_yes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("gateway.toml");

        let (result, output) = wizard(root.path(), "no\n\n\nno\nno\n\nno\n");
        result.unwrap();
        assert!(output.contains("No settings were written."));
        assert!(!path.exists());

        // Input that ends early, as when the terminal closes.
        let (result, _) = wizard(root.path(), "no\n\n\nno\n");
        assert!(result.unwrap_err().to_string().contains("not written"));
        assert!(!path.exists());
    }

    #[test]
    fn settings_that_are_not_valid_stop_setup_before_any_question() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("gateway.toml"), "[rch]\nenabled = true\n").unwrap();
        let (result, output) = wizard(root.path(), "");
        assert!(format!("{:#}", result.unwrap_err()).contains("fix or move it"));
        assert!(output.is_empty());
    }
}
