//! Manage provider profiles and try them against real APIs.
//!
//! Keys are typed at a hidden prompt and saved to Windows Credential Manager;
//! they are never accepted as command-line arguments, which would leave them
//! in shell history.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use clap::{Parser, Subcommand};
use providers::{
    AudioInput, ImageInput, KeyStore, Profile, ProviderClient, ProviderKind, Role, Settings,
    SettingsStore, SystemKeyStore, TextRequest, TranscribeRequest, http_client,
};

#[derive(Parser)]
#[command(about = "Manage Clio Notes provider profiles and test them against real APIs")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show profiles, role assignments and where settings are stored.
    List,
    /// Add a profile. Prompts for the API key unless --no-key is given.
    Add {
        #[arg(long)]
        name: String,
        /// anthropic or openai (any OpenAI-compatible service).
        #[arg(long)]
        kind: ProviderKind,
        /// Default: claude-opus-5-5 for anthropic; required for openai.
        #[arg(long)]
        model: Option<String>,
        /// Only for proxies or OpenAI-compatible services other than OpenAI.
        #[arg(long)]
        base_url: Option<String>,
        /// Reasoning effort, such as low, medium or high.
        #[arg(long)]
        effort: Option<String>,
        /// Send max_tokens instead of max_completion_tokens.
        #[arg(long)]
        legacy_max_tokens: bool,
        /// Do not ask for a key (local servers).
        #[arg(long)]
        no_key: bool,
    },
    /// Replace the saved API key of a profile.
    SetKey { profile: String },
    /// Delete a profile and its saved key.
    Remove { profile: String },
    /// Assign a profile to a role (speech, vision or text). Use "none" to clear.
    Assign { role: Role, profile: String },
    /// Check a profile's key and model without generating anything (free).
    Test { profile: String },
    /// List the models a profile's key can use.
    Models { profile: String },
    /// Generate text. Uses the profile assigned to the text role by default.
    Generate {
        prompt: String,
        #[arg(long)]
        profile: Option<String>,
    },
    /// Describe a screenshot. Uses the vision role's profile by default.
    Describe {
        image: PathBuf,
        #[arg(long)]
        profile: Option<String>,
    },
    /// Transcribe a WAV file. Uses the speech role's profile by default.
    Transcribe {
        audio: PathBuf,
        #[arg(long)]
        profile: Option<String>,
        /// Comma-separated terms likely to be spoken.
        #[arg(long, value_delimiter = ',')]
        keywords: Vec<String>,
        #[arg(long, default_value = "en")]
        language: String,
    },
}

type CliResult = Result<(), String>;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli.command).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(command: Command) -> CliResult {
    let store = SettingsStore::default_location().map_err(|e| e.to_string())?;
    let mut settings = store.load().map_err(|e| e.to_string())?;
    let keys = SystemKeyStore;
    let http = http_client();

    match command {
        Command::List => list(&store, &settings, &keys),

        Command::Add {
            name,
            kind,
            model,
            base_url,
            effort,
            legacy_max_tokens,
            no_key,
        } => {
            let model = match (model, kind) {
                (Some(m), _) => m,
                (None, ProviderKind::Anthropic) => providers::anthropic::DEFAULT_MODEL.into(),
                (None, ProviderKind::OpenaiCompatible) => {
                    return Err("--model is required for openai profiles. \
                                Use gpt-transcribe for speech; run `models` on a \
                                profile to see the chat models your key can use."
                        .into());
                }
            };
            let mut profile = Profile::new(name, kind, model);
            profile.base_url = base_url;
            profile.effort = effort;
            profile.legacy_max_tokens = legacy_max_tokens;
            settings
                .upsert_profile(profile.clone())
                .map_err(|e| e.to_string())?;
            if !no_key && profile.requires_key() {
                let key = prompt_key(&profile.name)?;
                keys.set(&profile.id, &key).map_err(|e| e.to_string())?;
            }
            store.save(&settings).map_err(|e| e.to_string())?;
            println!("Added \"{}\" ({}).", profile.name, profile.model);
            println!("Next: provider-check test \"{}\"", profile.name);
            Ok(())
        }

        Command::SetKey { profile } => {
            let p = find(&settings, &profile)?;
            let key = prompt_key(&p.name)?;
            keys.set(&p.id, &key).map_err(|e| e.to_string())?;
            println!("Saved the key for \"{}\".", p.name);
            Ok(())
        }

        Command::Remove { profile } => {
            let p = find(&settings, &profile)?.clone();
            settings.remove_profile(&p.id);
            store.save(&settings).map_err(|e| e.to_string())?;
            keys.delete(&p.id).map_err(|e| e.to_string())?;
            println!("Removed \"{}\" and its saved key.", p.name);
            Ok(())
        }

        Command::Assign { role, profile } => {
            let id = if profile.eq_ignore_ascii_case("none") {
                None
            } else {
                Some(find(&settings, &profile)?.id.clone())
            };
            settings
                .assign(role, id.as_deref())
                .map_err(|e| e.to_string())?;
            store.save(&settings).map_err(|e| e.to_string())?;
            match settings.assigned(role) {
                Some(p) => println!("{}: {} ({})", role.label(), p.name, p.model),
                None => println!("{}: not assigned", role.label()),
            }
            Ok(())
        }

        Command::Test { profile } => {
            let p = find(&settings, &profile)?;
            let started = Instant::now();
            let client = ProviderClient::for_profile(p, &keys, &http).map_err(|e| e.to_string())?;
            let r = client.check().await.map_err(|e| e.to_string())?;
            println!(
                "OK: \"{}\" can use {}{} ({} ms)",
                p.name,
                r.model,
                r.display_name
                    .map(|d| format!(" ({d})"))
                    .unwrap_or_default(),
                started.elapsed().as_millis()
            );
            Ok(())
        }

        Command::Models { profile } => {
            let p = find(&settings, &profile)?;
            let client = ProviderClient::for_profile(p, &keys, &http).map_err(|e| e.to_string())?;
            for id in client.list_models().await.map_err(|e| e.to_string())? {
                println!("{id}");
            }
            Ok(())
        }

        Command::Generate { prompt, profile } => {
            let p = resolve(&settings, profile.as_deref(), Role::Text)?;
            let generator = ProviderClient::for_profile(p, &keys, &http)
                .map_err(|e| e.to_string())?
                .into_text_generator();
            let started = Instant::now();
            let r = generator
                .generate(&TextRequest::new(prompt).max_tokens(4000))
                .await
                .map_err(|e| e.to_string())?;
            println!("{}", r.text);
            eprintln!(
                "-- {} | {} in / {} out tokens | {} ms",
                r.model,
                r.usage.input_tokens,
                r.usage.output_tokens,
                started.elapsed().as_millis()
            );
            Ok(())
        }

        Command::Describe { image, profile } => {
            let p = resolve(&settings, profile.as_deref(), Role::Vision)?;
            let data = std::fs::read(&image)
                .map_err(|e| format!("cannot read {}: {e}", image.display()))?;
            let media_type = match image.extension().and_then(|e| e.to_str()) {
                Some(e) if e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg") => {
                    "image/jpeg"
                }
                _ => "image/png",
            };
            let generator = ProviderClient::for_profile(p, &keys, &http)
                .map_err(|e| e.to_string())?
                .into_text_generator();
            let request = TextRequest::new(
                "This is a screenshot from a meeting. In at most 5 short bullet points, \
                 say what is on screen: the app or document, the title, and any key \
                 text, numbers or terms.",
            )
            .image(ImageInput {
                media_type: media_type.into(),
                data,
            })
            .max_tokens(4000);
            let started = Instant::now();
            let r = generator
                .generate(&request)
                .await
                .map_err(|e| e.to_string())?;
            println!("{}", r.text);
            eprintln!(
                "-- {} | {} in / {} out tokens | {} ms",
                r.model,
                r.usage.input_tokens,
                r.usage.output_tokens,
                started.elapsed().as_millis()
            );
            Ok(())
        }

        Command::Transcribe {
            audio,
            profile,
            keywords,
            language,
        } => {
            let p = resolve(&settings, profile.as_deref(), Role::SpeechToText)?;
            let data = std::fs::read(&audio)
                .map_err(|e| format!("cannot read {}: {e}", audio.display()))?;
            let stt = ProviderClient::for_profile(p, &keys, &http)
                .map_err(|e| e.to_string())?
                .into_speech_to_text()
                .ok_or_else(|| format!("\"{}\" cannot transcribe audio.", p.name))?;
            let mut request = TranscribeRequest::new(AudioInput::wav(data));
            request.language = Some(language);
            request.keywords = keywords;
            let started = Instant::now();
            let t = stt.transcribe(&request).await.map_err(|e| e.to_string())?;
            if t.segments.is_empty() {
                println!("{}", t.text);
            } else {
                for s in &t.segments {
                    println!("[{:>7.2}s] {}", s.start_ms as f64 / 1000.0, s.text);
                }
            }
            eprintln!("-- {} | {} ms", p.model, started.elapsed().as_millis());
            Ok(())
        }
    }
}

fn find<'a>(settings: &'a Settings, reference: &str) -> Result<&'a Profile, String> {
    settings
        .profile(reference)
        .ok_or_else(|| format!("No profile named \"{reference}\". Run `provider-check list`."))
}

fn resolve<'a>(
    settings: &'a Settings,
    reference: Option<&str>,
    role: Role,
) -> Result<&'a Profile, String> {
    match reference {
        Some(r) => find(settings, r),
        None => settings.assigned(role).ok_or_else(|| {
            format!(
                "No profile is assigned to {}. Pass --profile or run `provider-check assign`.",
                role.label().to_lowercase()
            )
        }),
    }
}

fn prompt_key(profile_name: &str) -> Result<String, String> {
    let key = rpassword::prompt_password(format!("API key for \"{profile_name}\" (hidden): "))
        .map_err(|e| format!("cannot read the key: {e}"))?;
    if key.trim().is_empty() {
        return Err("no key entered".into());
    }
    Ok(key)
}

fn list(store: &SettingsStore, settings: &Settings, keys: &dyn KeyStore) -> CliResult {
    println!("Settings file: {}", store.path().display());
    if settings.profiles.is_empty() {
        println!("No profiles yet. Add one with `provider-check add`.");
        return Ok(());
    }
    println!("Profiles:");
    for p in &settings.profiles {
        let key = match keys.get(&p.id) {
            Ok(Some(_)) => "key saved",
            Ok(None) if !p.requires_key() => "no key needed",
            Ok(None) => "NO KEY",
            Err(_) => "key store error",
        };
        println!(
            "  {:<20} {:<18} {:<28} {}{}",
            p.name,
            p.kind.label(),
            p.model,
            key,
            p.base_url
                .as_deref()
                .map(|u| format!("  [{u}]"))
                .unwrap_or_default()
        );
    }
    println!("Roles:");
    for role in Role::ALL {
        let who = settings
            .assigned(role)
            .map(|p| format!("{} ({})", p.name, p.model))
            .unwrap_or_else(|| "not assigned".into());
        println!("  {:<16} {who}", role.label());
    }
    Ok(())
}
