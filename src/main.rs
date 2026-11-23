use intake_agent::{SCHEMA_VERSION, library, organization, plan, recording, scan, speech, video};
use io::Read;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{env, io, path::PathBuf, process::ExitCode};

const USAGE: &str = "Usage:
  intake-agent diagnostics
  intake-agent scan <absolute-root> <relative-file>...
  intake-agent plan <absolute-source-root> <absolute-destination-root> <relative-destination-dir|.> <relative-file>...
  intake-agent narration-delivery-plan <absolute-stage-root> <absolute-destination-root> <absolute-stage-receipt.json>
  intake-agent organize <absolute-source-root> <absolute-destination-root> <decisions.json>
  intake-agent photo-evidence <absolute-root> <relative-file>...
  intake-agent video-inventory <absolute-root> <relative-file>...
  intake-agent video-resume <absolute-root> <absolute-checkpoint> <relative-file>...
  intake-agent video-archive-plan <absolute-source-root> <absolute-destination-root> <inventory.json> <relative-file>...
  intake-agent audio-plan <absolute-source-root> <relative-recording> <absolute-destination-root> <relative-output.mp3> <auto|audio-ordinal> <archival|high|portable>
  intake-agent transcribe <absolute-root> <relative-input.mp3> <auto|language-code>
  intake-agent align-notes <rough-notes.txt> <intake-transcript.json>
  intake-agent clip-plan <absolute-source-root> <relative-recording> <absolute-package-root> <selected-ranges.json>
  intake-agent library-import-plan <absolute-legacy-book-directory> <absolute-destination-book-directory>
  intake-agent library-listening-plan <absolute-book-directory> <absolute-listening-directory> <chapter-number>...
  intake-agent library-text-plan <absolute-book-directory> <absolute-legacy-text-catalog-directory> <absolute-destination-catalog-directory>
  intake-agent library-text-search <absolute-book-directory> <absolute-text-catalog-directory> <query>
  intake-agent library-transcript-plan <absolute-book-directory> <absolute-legacy-transcript-directory> <absolute-destination-catalog-directory>
  intake-agent library-transcript-search <absolute-book-directory> <absolute-transcript-catalog-directory> <query>
  intake-agent speech-preview <absolute-root> <relative-text.txt> <voice-id>
  intake-agent photo-suggest <absolute-root> <relative-photo> <installed-model:tag>
  intake-agent caption-plan <absolute-root> <decisions.json>
  intake-agent duplicates <absolute-root> <relative-file>...
  intake-agent verify <plan.json> <absolute-source-root> <absolute-destination-root> <action-id>...
  intake-agent apply <plan.json> <absolute-source-root> <absolute-destination-root> <absolute-new-journal> <action-id>...
  intake-agent receipt <absolute-journal>
  intake-agent verify-package <absolute-journal> <absolute-source-root> <absolute-package-root>
  intake-agent recover <absolute-journal> <absolute-source-root> <absolute-destination-root>
  intake-agent undo-plan <absolute-journal> <absolute-root> <original-action-id>...";

const RPC_VERSION: u32 = 1;
const RPC_READ_COMMANDS: &[&str] = &[
    "diagnostics",
    "scan",
    "plan",
    "narration-delivery-plan",
    "organize",
    "photo-evidence",
    "photo-suggest",
    "video-inventory",
    "video-archive-plan",
    "audio-plan",
    "transcribe",
    "align-notes",
    "clip-plan",
    "verify-package",
    "library-import-plan",
    "library-listening-plan",
    "library-text-plan",
    "library-text-search",
    "library-transcript-plan",
    "library-transcript-search",
    "speech-preview",
    "caption-plan",
    "duplicates",
    "verify",
    "receipt",
    "undo-plan",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RpcRequest {
    protocol_version: u32,
    operation: String,
    arguments: Vec<PathBuf>,
}

fn rpc(input: impl Read) -> io::Result<(Value, bool)> {
    const INPUT_LIMIT: u64 = 8 * 1024 * 1024;
    let invalid_input = |message| io::Error::new(io::ErrorKind::InvalidInput, message);
    let mut bytes = Vec::new();
    input.take(INPUT_LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > INPUT_LIMIT {
        return Err(invalid_input("Subprocess request exceeds 8 MiB"));
    }
    let request: RpcRequest = serde_json::from_slice(&bytes)
        .map_err(|_| invalid_input("Malformed subprocess request"))?;
    if request.protocol_version != RPC_VERSION {
        return Err(invalid_input("Unsupported subprocess protocol version"));
    }
    let allow_linux_execution = cfg!(target_os = "linux")
        && env::var_os("INTAKE_ALLOW_EXECUTION").is_some_and(|value| value == "1");
    let execute = (cfg!(windows) || allow_linux_execution)
        && matches!(request.operation.as_str(), "apply" | "recover");
    if !execute && !RPC_READ_COMMANDS.contains(&request.operation.as_str()) {
        return Err(invalid_input("Subprocess operation is unavailable"));
    }
    let mut arguments = Vec::with_capacity(request.arguments.len() + 1);
    arguments.push(PathBuf::from(request.operation));
    arguments.extend(request.arguments);
    let (record, complete) = run(&arguments, execute)?;
    Ok((
        json!({"protocol_version": RPC_VERSION, "record": record}),
        complete,
    ))
}

#[cfg(any(windows, target_os = "linux"))]
fn selected(ids: &[PathBuf]) -> io::Result<Vec<u32>> {
    ids.iter()
        .map(|id| {
            id.to_str().and_then(|id| id.parse().ok()).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "Expected a numeric action ID")
            })
        })
        .collect()
}

fn ffprobe_path() -> io::Result<PathBuf> {
    env::var_os("INTAKE_FFPROBE")
        .map(PathBuf::from)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Set INTAKE_FFPROBE to the absolute path of ffprobe",
            )
        })
}

fn run(arguments: &[PathBuf], _adapter: bool) -> io::Result<(Value, bool)> {
    #[cfg(windows)]
    let approval = if _adapter {
        intake_agent::execution::Approval::Desktop
    } else {
        intake_agent::execution::Approval::Terminal
    };
    #[cfg(target_os = "linux")]
    let approval = if _adapter {
        intake_agent::execution::Approval::ControllingTerminal
    } else {
        intake_agent::execution::Approval::Terminal
    };
    match arguments {
        [command] if command.as_os_str() == "diagnostics" => {
            let mut commands = vec![
                "diagnostics",
                "scan",
                "plan",
                "narration-delivery-plan",
                "organize",
                "photo-evidence",
                "video-inventory",
                "video-archive-plan",
                "audio-plan",
                "transcribe",
                "align-notes",
                "library-import-plan",
                "library-listening-plan",
                "library-text-plan",
                "library-text-search",
                "library-transcript-plan",
                "library-transcript-search",
                "speech-preview",
                "photo-suggest",
                "caption-plan",
                "duplicates",
            ];
            if cfg!(any(windows, target_os = "linux")) {
                commands.extend([
                    "verify",
                    "apply",
                    "receipt",
                    "recover",
                    "undo-plan",
                    "video-resume",
                    "clip-plan",
                    "verify-package",
                ]);
            }
            Ok((
                json!({
                "schema_version": SCHEMA_VERSION,
                "version": env!("CARGO_PKG_VERSION"),
                "commands": commands
                }),
                true,
            ))
        }
        [command, root, selected @ ..] if command.as_os_str() == "scan" && !selected.is_empty() => {
            Ok((serde_json::to_value(scan(root, selected)?)?, true))
        }
        [command, source, destination] if command.as_os_str() == "library-import-plan" => Ok((
            serde_json::to_value(library::import_plan(source, destination)?)?,
            true,
        )),
        [command, book, destination, chapters @ ..]
            if command.as_os_str() == "library-listening-plan" =>
        {
            let chapters = chapters
                .iter()
                .map(|chapter| {
                    chapter
                        .to_str()
                        .and_then(|number| number.parse().ok())
                        .ok_or_else(|| {
                            io::Error::new(io::ErrorKind::InvalidInput, "Expected a chapter number")
                        })
                })
                .collect::<io::Result<Vec<_>>>()?;
            Ok((library::listening_plan(book, destination, &chapters)?, true))
        }
        [command, root, relative, voice] if command.as_os_str() == "speech-preview" => Ok((
            speech::preview(root, relative, voice.to_str().unwrap_or_default())?,
            true,
        )),
        [command, book, source, destination] if command.as_os_str() == "library-text-plan" => {
            Ok((library::text_import_plan(book, source, destination)?, true))
        }
        [command, book, source, destination]
            if command.as_os_str() == "library-transcript-plan" =>
        {
            Ok((
                library::transcript_import_plan(book, source, destination)?,
                true,
            ))
        }
        [command, book, catalog, query]
            if command.as_os_str() == "library-text-search"
                || command.as_os_str() == "library-transcript-search" =>
        {
            let query = query.to_str().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "Expected a UTF-8 search query")
            })?;
            let result = if command.as_os_str() == "library-text-search" {
                library::search_text(book, catalog, query)?
            } else {
                library::search_transcripts(book, catalog, query)?
            };
            Ok((result, true))
        }
        [command, source, destination, directory, selected @ ..]
            if command.as_os_str() == "plan" && !selected.is_empty() =>
        {
            Ok((
                serde_json::to_value(plan(source, destination, directory, selected)?)?,
                true,
            ))
        }
        [command, stage, destination, receipt]
            if command.as_os_str() == "narration-delivery-plan" =>
        {
            Ok((
                serde_json::to_value(intake_agent::narration_delivery_plan(
                    stage,
                    destination,
                    receipt,
                )?)?,
                true,
            ))
        }
        [command, source, destination, decisions] if command.as_os_str() == "organize" => Ok((
            serde_json::to_value(organization::organize(source, destination, decisions)?)?,
            true,
        )),
        [command, root, decisions] if command.as_os_str() == "caption-plan" => Ok((
            serde_json::to_value(organization::caption_plan(root, decisions)?)?,
            true,
        )),
        [command, root, selected @ ..] if command.as_os_str() == "photo-evidence" => Ok((
            serde_json::to_value(organization::photo_evidence(root, selected)?)?,
            true,
        )),
        [command, root, selected @ ..] if command.as_os_str() == "video-inventory" => {
            let inventory = video::inventory(root, selected, &ffprobe_path()?)?;
            let complete = inventory.complete();
            Ok((serde_json::to_value(inventory)?, complete))
        }
        #[cfg(any(windows, target_os = "linux"))]
        [command, root, checkpoint, selected @ ..] if command.as_os_str() == "video-resume" => {
            let inventory = video::resume(root, selected, &ffprobe_path()?, checkpoint)?;
            let complete = inventory.complete();
            Ok((serde_json::to_value(inventory)?, complete))
        }
        [command, source, destination, report, selected @ ..]
            if command.as_os_str() == "video-archive-plan" =>
        {
            Ok((
                serde_json::to_value(video::archive_plan(source, destination, report, selected)?)?,
                true,
            ))
        }
        [command, source, file, destination, output, track, quality]
            if command.as_os_str() == "audio-plan" =>
        {
            let track = if track.as_os_str() == "auto" {
                None
            } else {
                Some(
                    track
                        .to_str()
                        .and_then(|track| track.parse().ok())
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "Expected auto or a zero-based audio ordinal",
                            )
                        })?,
                )
            };
            let quality = recording::Mp3Quality::parse(quality.to_str().unwrap_or_default())?;
            Ok((
                serde_json::to_value(recording::audio_plan(
                    source,
                    file,
                    destination,
                    output,
                    track,
                    quality,
                )?)?,
                true,
            ))
        }
        [command, root, file, language] if command.as_os_str() == "transcribe" => Ok((
            recording::transcribe(root, file, language.to_str().unwrap_or_default())?,
            true,
        )),
        [command, notes, transcript] if command.as_os_str() == "align-notes" => {
            recording::align_notes(notes, transcript)
        }
        #[cfg(any(windows, target_os = "linux"))]
        [command, source, file, destination, selection] if command.as_os_str() == "clip-plan" => {
            Ok((
                serde_json::to_value(recording::clip_plan(source, file, destination, selection)?)?,
                true,
            ))
        }
        #[cfg(any(windows, target_os = "linux"))]
        [command, journal, source, destination] if command.as_os_str() == "verify-package" => Ok((
            intake_agent::execution::verify_package(journal, source, destination)?,
            true,
        )),
        [command, root, file, model] if command.as_os_str() == "photo-suggest" => {
            let model = model.to_str().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "Expected an Ollama model tag")
            })?;
            Ok((
                serde_json::to_value(organization::photo_suggest(root, file, model)?)?,
                true,
            ))
        }
        [command, root, selected @ ..] if command.as_os_str() == "duplicates" => Ok((
            serde_json::to_value(organization::duplicates(root, selected)?)?,
            true,
        )),
        #[cfg(any(windows, target_os = "linux"))]
        [command, plan, source, destination, ids @ ..] if command.as_os_str() == "verify" => Ok((
            intake_agent::execution::verify(plan, source, destination, &selected(ids)?)?,
            true,
        )),
        #[cfg(any(windows, target_os = "linux"))]
        [command, plan, source, destination, journal, ids @ ..]
            if command.as_os_str() == "apply" =>
        {
            intake_agent::execution::apply_with_approval(
                plan,
                source,
                destination,
                journal,
                &selected(ids)?,
                approval,
            )
        }
        #[cfg(any(windows, target_os = "linux"))]
        [command, journal] if command.as_os_str() == "receipt" => {
            Ok((intake_agent::execution::receipt(journal)?, true))
        }
        #[cfg(any(windows, target_os = "linux"))]
        [command, journal, source, destination] if command.as_os_str() == "recover" => {
            intake_agent::execution::recover_with_approval(journal, source, destination, approval)
        }
        #[cfg(any(windows, target_os = "linux"))]
        [command, journal, root, ids @ ..] if command.as_os_str() == "undo-plan" => Ok((
            serde_json::to_value(intake_agent::execution::undo_plan(
                journal,
                root,
                &selected(ids)?,
            )?)?,
            true,
        )),
        _ => Err(io::Error::new(io::ErrorKind::InvalidInput, USAGE)),
    }
}

fn main() -> ExitCode {
    let arguments: Vec<_> = env::args_os().skip(1).map(PathBuf::from).collect();
    if arguments.len() == 1 && arguments[0].as_os_str() == "--help" {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let result = if arguments.len() == 1 && arguments[0].as_os_str() == "rpc" {
        rpc(io::stdin().lock())
    } else {
        run(&arguments, false)
    };
    let result = result.and_then(|(record, complete)| {
        use io::Write;
        let mut output = io::stdout().lock();
        serde_json::to_writer_pretty(&mut output, &record)?;
        writeln!(output)?;
        Ok(complete)
    });
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(3),
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(if error.kind() == io::ErrorKind::InvalidInput {
                2
            } else {
                1
            })
        }
    }
}

#[cfg(test)]
mod rpc_tests {
    use super::*;

    #[test]
    fn subprocess_protocol_requires_version_and_exact_arguments() {
        let request = |version, operation| {
            serde_json::to_vec(&json!({
                "protocol_version": version, "operation": operation, "arguments": []
            }))
            .unwrap()
        };
        let (response, complete) = rpc(request(1, "diagnostics").as_slice()).unwrap();
        assert!(complete);
        assert_eq!(response["protocol_version"], 1);
        assert_eq!(response["record"]["schema_version"], SCHEMA_VERSION);
        for (version, operation) in [(0, "diagnostics"), (1, "apply"), (1, "recover"), (1, "rpc")] {
            assert_eq!(
                rpc(request(version, operation).as_slice())
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
        assert!(rpc(b"{ \"approval\": true }".as_slice()).is_err());
    }
}
