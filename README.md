# intake-agent

The CLI plans placements for explicitly selected files on Windows local NTFS and supported native Linux filesystems. On Windows local NTFS and user-owned native Linux btrfs it can verify a plan, execute a reviewed subset, report its durable state and recover an interrupted run.

Intake Agent helps you file, inventory, and transcribe selected media. File placements require a reviewed plan and explicit approval. The journal tracks progress so you can recover interrupted placements and request undo for a completed move if the files and paths still match. You choose when to run it.

## Set up with my AI

Copy the prompt below and paste it into an AI assistant with access to your local terminal. For manual setup, see [Core installation](#core-installation).

```text
Please set up the Intake Agent core CLI on my computer.

1. Use your local terminal to check my OS, architecture, and existing tools. If you only have a remote terminal or chat access, give me local setup instructions and tell me what I need to run.
2. Use the official v0.2.0 release at https://github.com/StudioDoNotDisturb/intake-agent/releases/tag/v0.2.0. Download intake-agent-0.2.0-source.zip from that release and verify its SHA-256 before extracting it. The expected hash is dcc16374900811392e83564d62949e1ed57123604470172b185ddda9b20a2452. Stop if the hash differs.
3. Follow the release README. The core CLI needs Rust 1.99 or newer, Cargo, and a host linker. Ask me before installing a missing prerequisite or making any system-level change. Build and install for my user account with cargo install --locked --path . from the extracted source directory. Leave Bun, MCP, FFmpeg, Python, and models for later.
4. Run intake-agent diagnostics. Create one sample file and two separate folders in temporary storage, then generate a plan for that one file. Leave the plan unapplied and keep my media folders untouched.
5. Tell me what you installed, where the executable is, and whether diagnostics and the sample plan succeeded. If setup is blocked, tell me exactly what is missing.
```

## What you need

The core tool needs only Rust. Each optional feature needs its own tools, which you install separately. The repository bundles no engines, models or media, and the toolkit downloads nothing for you.

| Feature | Commands | Also install |
|---|---|---|
| Plan, apply, recover and undo placements | `scan`, `plan`, `verify`, `apply`, `recover`, `receipt`, `undo-plan` | [Rust](https://rustup.rs) 1.99 or newer |
| Photo dates, filing, captions and duplicates | `photo-evidence`, `organize`, `caption-plan`, `duplicates` | Nothing more |
| Photo caption suggestions | `photo-suggest` | [Ollama](https://ollama.com) and a local vision model |
| Video inventory and archive plans | `video-inventory`, `video-resume`, `video-archive-plan` | ffprobe from [FFmpeg](https://ffmpeg.org/download.html) |
| MP3 extraction and clip packages | `audio-plan`, `clip-plan`, `verify-package` | FFmpeg and ffprobe built with `libmp3lame` and `libx264` |
| Transcription | `transcribe` | FFmpeg, Python and a Whisper model. CPU works without an NVIDIA GPU. |
| Rough-note timing review | `align-notes` | Nothing more; it reads a transcript |
| Narration review against a script | `src/whisper_review.py` | An NVIDIA GPU for script-based narration review, Python 3.12, FFmpeg, Faster Whisper and the narration model below |
| Audiobook library and search | `library-*` | Nothing more |
| Speech preview | `speech-preview` | Python and the Kokoro model files below |
| MCP and HTTP adapters | `adapter.ts` | [Bun](https://bun.sh) 1.4.4 |

The tested FFmpeg and ffprobe version was 8.1.1. Each section below gives the exact Python packages and environment variables.

### Tested models

These are the exact models used in testing. Other models may work, but only these were checked. Model files carry their own licenses.

| Use | Model | Get it | Check |
|---|---|---|---|
| Basic transcription | Whisper `tiny` (multilingual) | `& C:/tools/whisper/.venv/Scripts/python.exe -c "import whisper; whisper.load_model('tiny', download_root='C:/tools/whisper/models')"` | openai-whisper checks the download's SHA-256 itself |
| Narration review | [`dropbox-dash/faster-whisper-large-v3-turbo`](https://huggingface.co/dropbox-dash/faster-whisper-large-v3-turbo), revision `0a363e9161cbc7ed1431c9597a8ceaf0c4f78fcf` | `& C:/tools/faster-whisper/.venv/Scripts/hf.exe download dropbox-dash/faster-whisper-large-v3-turbo --revision 0a363e9161cbc7ed1431c9597a8ceaf0c4f78fcf --local-dir C:/tools/faster-whisper/large-v3-turbo` | `model.bin` SHA-256 `e76620f83d5f5b69efd3d87e3dc180c1bd21df9fbebacfd4335e5e1efcc018da` |
| Photo suggestions | `gemma3:4b` | `ollama pull gemma3:4b` | Tested on one synthetic image with an earlier build of this tag. The tag has since been updated, so a fresh pull gets a build that was not tested. Caption quality on real photos is unmeasured. |
| Speech preview | Kokoro v1.0 ONNX and voice pack | [`kokoro-v1.0.onnx`](https://github.com/thewh1teagle/kokoro-onnx/releases/download/model-files-v1.0/kokoro-v1.0.onnx) and [`voices-v1.0.bin`](https://github.com/thewh1teagle/kokoro-onnx/releases/download/model-files-v1.0/voices-v1.0.bin) from the kokoro-onnx `model-files-v1.0` release | 325,532,387 and 28,214,398 bytes |

The `hf` command comes with the Faster Whisper environment. Run `(Get-FileHash <file>).Hash` to compare a downloaded file with the listed SHA-256.

## Core installation

Install Rust 1.99 or newer with Cargo and a linker for your host, then run `cargo install --locked --path .` from the checkout. The core build works without Bun, Python, FFmpeg or model weights. Use `cargo build --locked` for a development binary under `target/debug/`.

```powershell
cargo build --locked
cargo run --locked -- diagnostics
cargo run --locked -- scan 'C:\fixtures\incoming' 'a.txt' 'batch\b.txt'
cargo run --locked -- plan 'C:\fixtures\incoming' 'C:\fixtures\library' . 'batch\b.txt'
cargo run --locked -- plan 'C:\fixtures\incoming' 'C:\fixtures\incoming' sorted 'a.txt'
```

Use existing directories and explicitly name each regular file relative to its source root. A plan preserves basenames and places every selected file into the named existing destination directory. Use `.` for the destination root itself. Distinct roots must be disjoint. Existing output names and repeated output names are rejected, including case variants. Keep the roots and journal under your control. The path checks reject symlinks and Windows reparse points on inspected paths. Execution pins verified directories and checks source bytes and filesystem identities before mutation.

Structured results go to stdout. Diagnostics, errors and the approval prompt go to stderr. Exit 0 means success, 1 means an I/O or serialization error, 2 means the command or validation was rejected, and 3 means a partial result. `receipt` exits 0 when it can read the journal. Check its `status` and `torn_tail` fields. Run `intake-agent --help` for the complete CLI command and argument reference. `intake-agent diagnostics` reports the command inventory as JSON.

The plan contains `body` and `sha256`. The digest covers the compact UTF-8 JSON encoding of `body` produced by the pinned serde_json without an ending newline, with lexicographically sorted object keys and preserved array order. It binds both canonical roots, ordered action IDs, each source's path, size and hash, and the relative destination for each action. Schema version 2 also records filesystem identities on Windows and Linux. Regenerate version 1 plans. Paths use host-native syntax. Windows output uses canonical extended paths. Windows command arguments take ordinary absolute drive paths. Reordering inputs changes action IDs and the digest.

## Verify, apply and recover

In PowerShell 7, this sequence plans two files and selects action 2 for a cross-root copy.

```powershell
$tool = '.\target\debug\intake-agent.exe'
& $tool plan 'C:\fixtures\incoming' 'C:\fixtures\library' . 'a.txt' 'b.txt' > 'C:\fixtures\plan.json'
& $tool verify 'C:\fixtures\plan.json' 'C:\fixtures\incoming' 'C:\fixtures\library' 2
& $tool apply 'C:\fixtures\plan.json' 'C:\fixtures\incoming' 'C:\fixtures\library' 'C:\fixtures\run.jsonl' 2
& $tool receipt 'C:\fixtures\run.jsonl'
& $tool recover 'C:\fixtures\run.jsonl' 'C:\fixtures\incoming' 'C:\fixtures\library'
```

`verify` checks the digest, roots and selected actions without mutation. `apply` requires interactive stdin and stderr. Review the displayed paths, then type the exact `APPROVE` line, including the digest and selected action IDs. Use a new absolute journal path outside both roots. A same-root, same-volume action moves the file. A copy stages and verifies the primary file bytes before publication and retains the source. Both methods refuse to overwrite an existing destination.

Use `receipt` to inspect an interrupted or failed run. `recover` requires the original roots and asks for the displayed `RECOVER` line before resuming an incomplete run. Recovery checks file identity and content, preserves completed actions and handles an incomplete final journal record. When stage creation was interrupted before the journal recorded its file identity, recovery reports a conflict and leaves the stage in place for inspection. A journal interrupted within its initial header can be unreadable. The header is synced before any media mutation. Preserve the journal and any staging files until the run is reconciled.

The acceptance fixtures cover local NTFS and native Linux btrfs process termination, same-root moves and copies between disjoint roots on one volume. Windows inspection and execution reject UNC and mapped network roots because a server can follow links without exposing them as Windows reparse points. Power loss and distinct physical volumes remain unverified.

On a native btrfs filesystem, the CLI and subprocess RPC returned identical scan records. The Linux worker produced both placement policies and rejected file links, directory links and collisions. To cross-build the static x86-64 worker from Windows PowerShell, run the following commands.

```powershell
rustup target add x86_64-unknown-linux-musl
$env:CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER = 'rust-lld'
$env:CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS = '-C linker-flavor=ld.lld -C link-self-contained=yes'
cargo build --locked --release --target x86_64-unknown-linux-musl
```

For Linux execution, set `INTAKE_EXECUTION_ROOT` to an existing private btrfs directory owned by the invoking user. Put the permitted data roots, plan, and journals beneath that one directory. Keep journals outside both data roots. The anchor and all execution directories beneath it need mode `0700` and ordinary Unix permissions. Check the permissions after creating each directory because inherited share ACLs can override the shell umask. Keep the anchor and its parent path under your control. Use one anchor for overlapping work and keep other writers away from the files and directories during execution. The worker rejects hard-linked files and files writable by other users. Use an interactive terminal for Linux approval and recovery.

To repeat the btrfs interruption suite, cross-build the library test binary with `cargo test --locked --target x86_64-unknown-linux-musl --lib --no-run` using the same linker variables. Run the reported binary on the btrfs host with `--include-ignored --skip interruption_worker --test-threads=1`. Set `INTAKE_EXECUTION_ROOT` to a disposable private btrfs directory and `TMPDIR` to a private subdirectory before launch. Use `umask 077` so the fixture creates private directories. The test parent kills its own fixture children around publication and receipt updates, then checks the exact source, destination, and journal state.

## File a photo batch by date

Create the destination folders under the permitted root. For example, to file `portrait.png` under `portraits/2024/02/29`, put this array in `C:\fixtures\decisions.json` outside the photo root.

```json
[
  { "file": "portrait.png", "date": "2024-02-29", "category": "portraits" }
]
```

Each item uses a relative filename, a reviewed calendar date and an optional single category. The resulting directory is `[category/]YYYY/MM/DD`. An optional `name` renames the file to a safe basename with the same extension. Create the full directory before planning. Filing works without video, vision or library software.

Run `photo-evidence <absolute-root> <relative-file>...` to inspect embedded EXIF before choosing a date. Each result pairs its metadata with the hash of the same file. It reports the raw date and offset fields, a suggested local calendar date from `DateTimeOriginal` when usable, and a conflict when valid capture and digitization dates disagree. Missing, malformed or oversized EXIF leaves the choice to the reviewer. Files over 32 MiB are hashed but skip EXIF parsing. Filesystem modification time is displayed as separate evidence. Put the reported hash in the optional `expected_sha256` decision field to require that the bytes still match your review when generating the plan.

```powershell
$root = 'C:\fixtures\photos'
& $tool organize $root $root 'C:\fixtures\decisions.json' > 'C:\fixtures\photos-plan.json'
& $tool verify 'C:\fixtures\photos-plan.json' $root $root 1
& $tool apply 'C:\fixtures\photos-plan.json' $root $root 'C:\fixtures\photos-run.jsonl' 1
& $tool receipt 'C:\fixtures\photos-run.jsonl'
```

`duplicates <absolute-root> <relative-file>...` groups the explicitly selected files by byte length and full SHA-256. It produces a review report without moving or deleting files.

### Get optional local vision suggestions

```powershell
& $tool photo-suggest $root 'portrait.png' 'gemma3:4b'
```

Start a trusted local Ollama instance on `127.0.0.1:11434` and select an installed vision model by its exact `name:tag`. Each call accepts one PNG or JPEG up to 16 MiB. It returns a proposed caption and category, model-reported confidence and ambiguities, plus the photo hash and installed model digest. The report is read-only. Copy reviewed fields and the photo's SHA-256 into your own filing or caption decisions before planning an action.

The command checks the provider's installed model inventory before sending image bytes and refuses remote model entries. Generation has a 90-second timeout. A missing model, unavailable provider or invalid response produces an error. The manual workflow works independently of Ollama.

### Write reviewed caption sidecars

Put decisions in a JSON file outside the photo root. Each entry contains a relative photo path, the exact caption text and an optional `expected_sha256` from photo evidence.

```json
[{"file":"portraits/2024/02/29/portrait.png","caption":"Portrait at sunset."}]
```

```powershell
& $tool caption-plan $root 'C:\fixtures\captions.json' > 'C:\fixtures\captions-plan.json'
& $tool verify 'C:\fixtures\captions-plan.json' $root $root 1
& $tool apply 'C:\fixtures\captions-plan.json' $root $root 'C:\fixtures\captions-run.jsonl' 1
```

Use a separate selection and journal for captions after any placement or rename. Each approved action writes UTF-8 text to `<photo filename>.caption.txt` beside the photo. Publication preserves existing sidecars. For an interrupted caption run, use `recover` with the caption journal and the photo root for both root arguments.

### Undo a completed move

`undo-plan` accepts the original journal, root and original IDs. It verifies each completed same-root move against the recorded file identity and content and requires its original destination to be free. It returns a fresh plan with action IDs starting at 1. Review those IDs and use a distinct journal for the reverse run.

```powershell
& $tool undo-plan 'C:\fixtures\photos-run.jsonl' $root 1 > 'C:\fixtures\undo-plan.json'
& $tool verify 'C:\fixtures\undo-plan.json' $root $root 1
& $tool apply 'C:\fixtures\undo-plan.json' $root $root 'C:\fixtures\undo-run.jsonl' 1
```

Use `recover` on the undo journal if the reverse run is interrupted. Copy-retain actions are ineligible for undo. Keep input roots operator-controlled, review the displayed paths and digest at the terminal, and inspect the run receipt after completion or failure.

## Inventory selected video files

Point `INTAKE_FFPROBE` to a trusted local ffprobe executable. The local fixture used FFmpeg and ffprobe 8.1.1. Video inventory is optional and accepts up to 32 explicitly named paths per batch.

```powershell
$env:INTAKE_FFPROBE = 'C:/tools/ffmpeg/bin/ffprobe.exe'
intake-agent video-inventory C:/fixtures/media 'camera-a/2024-02-29/take.mp4' 'camera-b/20240301/take.mp4'
```

Each clip has a root-relative path, full-file SHA-256, file identity on supported platforms and bounded ffprobe JSON. The JSON preserves stream indices, codecs, exact rational rates and time bases, duration, and rotation tags or display-matrix side data. A distinct audio flag records whether ffprobe found an audio stream. Use packet timestamp intervals to confirm variable frame rate when the rate fields differ. [ffprobe documentation](https://ffmpeg.org/ffprobe.html)

Capture dates record raw embedded values and validated dates found in the relative path. When their calendar dates disagree, review the evidence before using a filing date. Filesystem modification time is low-confidence evidence and is kept as an epoch timestamp. Files with probe errors or no video stream still have hashes and a per-file status. Exit code `3` signals incomplete probe coverage. Missing ffprobe leaves the other intake commands available.

### Resume an inventory and verify an archive plan

```powershell
intake-agent video-resume C:/fixtures/media C:/fixtures/work/ffprobe.jsonl 'camera-b/20240301/take.mp4' > C:/fixtures/work/video.json
intake-agent video-archive-plan C:/fixtures/media C:/fixtures/archive C:/fixtures/work/video.json 'camera-b/20240301/take.mp4' > C:/fixtures/work/archive.json
intake-agent verify C:/fixtures/work/archive.json C:/fixtures/media C:/fixtures/archive 1
```

Keep the checkpoint in an existing operator-controlled directory outside the media root. On Linux it must be under `INTAKE_EXECUTION_ROOT`. Re-run the same command after an interruption. The checkpoint synchronizes each completed probe and retries failed probes. The command hashes each selected source again and reuses a probe only when the source and configured ffprobe executable hashes match. Report fields `ffprobe_sha256` and `reused_probes` show the executable identity and reuse count. Keep the checkpoint and generated inventory under your control.

Select standalone videos explicitly and create the `originals/by-date/YYYY/YYYY-MM` directories before planning. The planner validates each selection against the saved file identity and SHA-256 and derives its date from the probe and relative path. Probe errors, conflicting or weak dates, content duplicates in the inventory, destination collisions and Live Photo signals require separate review. The inventory records both the Apple content identifier and the `mebx` stream signal. Use the normal `verify`, `apply`, `receipt` and `recover` commands with the generated plan. Archive placement in a distinct root copies and retains the source. A plan using the exact same root uses the usual same-filesystem move policy.

The toolkit passes ffprobe the seekable, read-only file handle and limits it to the `fd` protocol and an allowlist of local container formats. MP4 was verified on Windows, including a clip with metadata at the end of the file. The probe has a per-file timeout and output limit. The CLI provides the writable `video-resume` checkpoint.

### Extract an approved MP3 from a recording

```powershell
$env:INTAKE_FFMPEG = 'C:/tools/ffmpeg/bin/ffmpeg.exe'
$env:INTAKE_FFPROBE = 'C:/tools/ffmpeg/bin/ffprobe.exe'
intake-agent audio-plan C:/fixtures/media take.mp4 C:/fixtures/audio take.mp3 auto high > C:/fixtures/work/audio.json
intake-agent verify C:/fixtures/work/audio.json C:/fixtures/media C:/fixtures/audio 1
intake-agent apply C:/fixtures/work/audio.json C:/fixtures/media C:/fixtures/audio C:/fixtures/work/audio-run.jsonl 1
intake-agent receipt C:/fixtures/work/audio-run.jsonl
```

Use installed local FFmpeg and ffprobe builds with the `fd` protocol and the `libmp3lame` encoder. Selection `auto` chooses the first default audio stream, or the first audio stream if none is default. An explicit zero-based ordinal counts across audio streams only. The verified plan shows the actual container stream index and binds both executable hashes. Choose `archival` (VBR quality 0), `high` (VBR quality 2) or `portable` (160 kbps). Extraction encodes a single MP3 stream at the closest supported sample rate and retains mono or uses stereo. The output carries technical encoder metadata only.

Use an existing destination parent and a new `.mp3` path. The approved action retains the source, validates the staged MP3 and publishes without replacing another file. The journal records the source and derived output SHA-256 and byte counts. Use the usual `recover` path for an interrupted run.

### Transcribe the extracted audio locally

```powershell
$env:INTAKE_WHISPER_PYTHON = 'C:/tools/whisper/.venv/Scripts/python.exe'
$env:INTAKE_WHISPER_MODEL = 'C:/tools/whisper/models/tiny.pt'
intake-agent transcribe C:/fixtures/audio take.mp3 en > C:/fixtures/work/take-transcript.json
```

Use a trusted local Python environment with `openai-whisper`, Torch and NumPy, and a trusted local Whisper checkpoint. This CPU workflow accepts a selected MP3 of up to one hour and uses the same configured FFmpeg to decode mono PCM. Use a lowercase language code or `auto` for detection. Verified versions were openai-whisper 20250625, Torch 2.12.0 and NumPy 2.4.6 with the multilingual tiny checkpoint. Model installation is separate from transcription.

For long-form narration, the CLI also supports an opt-in Faster Whisper recipe. On Windows, create a separate Python 3.12 environment and point to the `model.bin` of a local CTranslate2 Whisper model. Keep `config.json`, `preprocessor_config.json`, `tokenizer.json` and `vocabulary.json` alongside the weights.

```powershell
py -3.12 -m venv C:/tools/faster-whisper/.venv
$env:INTAKE_WHISPER_PYTHON = 'C:/tools/faster-whisper/.venv/Scripts/python.exe'
& $env:INTAKE_WHISPER_PYTHON -m pip install faster-whisper==1.2.1 ctranslate2==4.9.0 numpy==2.5.5
$env:INTAKE_WHISPER_BACKEND = 'faster-whisper-narration'
$env:INTAKE_WHISPER_MODEL = 'C:/tools/faster-whisper/large-v3-turbo/model.bin'
intake-agent transcribe C:/fixtures/audio take.mp3 en > C:/fixtures/work/take-transcript.json
```

The narration recipe uses CPU INT8, beam size 5, word timestamps, and disables previous-text conditioning, VAD and silence-based skipping. It emits recipe version 2 and records the model component hashes. Clear the backend setting to select the original recipe, which emits version 1. Use the CLI for full chapters. HTTP and MCP requests have a shorter timeout.

These settings were chosen by measurement on synthesized narration. On a 2,228-word chapter, the CPU recipe with the tested large-v3-turbo model made 46 word errors (2.07%). Whisper `small.en` with FP16 on CUDA made 67 (3.01%). That chapter was used to choose the model, so expect different numbers on your own audio.

For NVIDIA GPU inference on 64-bit Windows, install the CUDA 12 runtime packages in the same Faster Whisper environment. The worker locates the DLLs in that environment. Use the same model files and select the CUDA recipe.

```powershell
& $env:INTAKE_WHISPER_PYTHON -m pip install nvidia-cublas-cu12==12.9.2.10 nvidia-cudnn-cu12==9.28.1.4 nvidia-cuda-nvrtc-cu12==12.9.86
$env:INTAKE_WHISPER_BACKEND = 'faster-whisper-narration-cuda'
intake-agent transcribe C:/fixtures/audio take.mp3 en > C:/fixtures/work/take-transcript.json
```

The CUDA recipe uses `int8_float16` with the same decoding options and emits recipe version 3. Selecting CUDA requires a working NVIDIA driver and fails if the runtime cannot use the GPU.

### Finish a new spoken-script job

Bring the approved spoken text for each chapter. You can supply recorded audio or configure a
local speech engine to render the chapter and start GPU review in the same command. The walkthrough
uses Kokoro to render chapters on the CPU and Whisper to review them on the GPU.

Use the CUDA narration environment above (`$env:INTAKE_WHISPER_PYTHON` and
`$env:INTAKE_WHISPER_MODEL`) and point `$env:INTAKE_FFMPEG` to your local `ffmpeg.exe`. The commands
below use PowerShell 7 from the toolkit checkout directory. Job workspaces and deliveries require a
filesystem with hard links. The standard job command locks the selected chapter list, original audio
and spoken scripts to a workspace. It blind-transcribes each chapter on the GPU, builds a timed
mismatch queue, reruns unresolved excerpts on the GPU and resumes from saved evidence.

Write each chapter script as the approved **spoken** text before synthesis: expand numbers,
ordinals, dates and abbreviations into the exact words the voice should speak (for example, write
`eighteen eighty-three` instead of `1883` and `Doctor` instead of `Dr.`) so blind transcription
compares spoken words fairly while preserving your formatting and punctuation. The standard job and
local repair accept chapter audio up to three hours.

Create a working folder (for example `C:/fixtures/narration`) and set these PowerShell variables:

```powershell
$env:INTAKE_FFMPEG = 'C:/tools/ffmpeg/bin/ffmpeg.exe'
$root = 'C:/fixtures/narration'
$manifest = "$root/chapters.json"
$workspace = "$root/work"
$stage = "$root/stage"            # Must NOT exist before job stages the verified set
$delivery = "$root/delivery"      # Existing destination folder for Rust apply
$rendererConfig = "$root/renderer.json"
New-Item -ItemType Directory -Force -Path $delivery | Out-Null
```

#### Option A. Start from scripts with Kokoro

From the toolkit checkout in PowerShell 7, install Kokoro in its own Python 3.14 environment.
The example uses the American English voice `am_michael` at normal speed.

```powershell
py -3.14 -m venv "$root/kokoro-python"
if ($LASTEXITCODE -ne 0) { throw 'Could not create the Kokoro environment' }
$kokoro = "$root/kokoro-python/Scripts/python.exe"
$requirements = (Resolve-Path ./kokoro-requirements.txt).Path
& $kokoro -m pip install -r $requirements
if ($LASTEXITCODE -ne 0) { throw 'Could not install the Kokoro dependencies' }
$assets = 'https://github.com/thewh1teagle/kokoro-onnx/releases/download/model-files-v1.0'
$model = "$root/kokoro-v1.0.onnx"
$voices = "$root/voices-v1.0.bin"
Invoke-WebRequest "$assets/kokoro-v1.0.onnx" -OutFile $model
Invoke-WebRequest "$assets/voices-v1.0.bin" -OutFile $voices
if ((Get-FileHash $model).Hash -ne '7d5df8ecf7d4b1878015a32686053fd0eebe2bc377234608764cc0ef3636a6c5' -or
    (Get-FileHash $voices).Hash -ne 'bca610b8308e8d99f32e6fe4197e7ec01679264efed0cac9140fe9c29f1fbf7d') {
    throw 'Kokoro model or voice pack SHA-256 did not match'
}
$worker = (Resolve-Path ./src/kokoro_worker.py).Path
@{
    command = @($kokoro, '-I', '-B', $worker, $model, $voices, 'am_michael', $requirements, '{script}', '{audio}')
} | ConvertTo-Json | Set-Content $rendererConfig -Encoding utf8
```

`command[0]` must be an absolute `.exe` path on Windows. Keep `{script}` and `{audio}` as separate
arguments. Intake Agent binds the renderer and existing absolute file arguments by SHA-256. For a
different engine, list any additional assets in `"inputs"`, relative to `renderer.json` or absolute. Create
`chapters.json` using 3-to-8-digit chapter IDs (`"001"`, `"002"`), omitting `"audio"` for rendered
chapters:

```json
{
  "chapters": {
    "001": {"reference": "chapter-01-spoken.txt"},
    "002": {"reference": "chapter-02-spoken.txt"}
  }
}
```

Run or resume the job (keep passing `--renderer $rendererConfig` on resume runs):

```powershell
$json = & $env:INTAKE_WHISPER_PYTHON src/whisper_review.py job $manifest $workspace `
  --model $env:INTAKE_WHISPER_MODEL --ffmpeg $env:INTAKE_FFMPEG `
  --renderer $rendererConfig --stage $stage
$exit = $LASTEXITCODE
$result = $json | ConvertFrom-Json
$exit
```

#### Option B: Start from existing chapter audio

Omit `--renderer` and supply `"audio"` (and optionally `"audio_sha256"` / `"reference_sha256"`) in
`chapters.json`:

```json
{
  "chapters": {
    "001": {"audio": "chapter-01.mp3", "reference": "chapter-01-spoken.txt"},
    "002": {"audio": "chapter-02.mp3", "reference": "chapter-02-spoken.txt"}
  }
}
```

```powershell
$json = & $env:INTAKE_WHISPER_PYTHON src/whisper_review.py job $manifest $workspace `
  --model $env:INTAKE_WHISPER_MODEL --ffmpeg $env:INTAKE_FFMPEG --stage $stage
```

#### Automatic retries, review decisions and repair

When a rendered chapter has discrepancies, `job` automatically attempts targeted snippet repair (for
up to 4 isolated items) and up to `--max-retries` (default `2`) fresh synthesis retries, selecting
the lowest-edit audio and stopping early if a deterministic renderer produces identical bytes or zero
edits. If any chapter still has pending items or nonzero edits, `job` prints JSON to stdout and
exits `2`.

Inspect `$result.chapters.'001'.decisions`, the clip WAVs under the chapter round's `clips/` folder,
and `$result.chapters.'001'.rerun` (`beam_size=10` blind CUDA transcript). `job` pre-creates
`decisions.json` with the exact `queue_sha256` already populated. Edit its `"decisions"` array:

```json
{
  "queue_sha256": "<leave the generated SHA-256 unchanged>",
  "decisions": [
    {
      "id": 1,
      "action": "replace",
      "evidence": "Listened to clips/001.wav; narrator clearly speaks Harbor.",
      "learn": true
    },
    {
      "id": 2,
      "action": "retain",
      "evidence": "Confirmed narrator omitted 'that'; audio needs repair."
    }
  ]
}
```

- Use `"action": "replace"` only when listening confirms the audio actually spoke the script words
  and Whisper misheard them. If the replacement changes word count, add `"timings": [[start, end], ...]`
  in chapter seconds within `[clip_start, clip_end]`.
- Add `"learn": true` on verified 1-to-4-word replacements to carry that pattern into later chapters
  in the same job (and into later jobs via `--learned "$workspace/learned.json"`). A carried rule is
  applied automatically only when the new chapter's fresh CUDA `rerun` on that clip confirms the
  expected words; otherwise it stays pending as a suggestion for your review.
- Use `"action": "retain"` for a genuine narration mistake so it stays visible and blocks delivery
  until repaired.

To supply a manually re-rendered or repaired replacement, point `"candidate"` in `chapters.json` to
the new audio file:

```json
{
  "chapters": {
    "001": {"reference": "chapter-01-spoken.txt", "candidate": "chapter-01-repaired.wav"}
  }
}
```

For an equal-duration interval swap (preserving downstream timestamps), `patch` splices a donor
interval into a new 24 kHz mono WAV without modifying the original:

```powershell
& $env:INTAKE_WHISPER_PYTHON src/whisper_review.py patch $original $donor $candidate `
  --ffmpeg $env:INTAKE_FFMPEG --start 331.16 --end 331.405 --donor-start 0
```

#### Playback check and Rust-approved delivery

Re-run the same `job` command until `$exit` is `0` (`$result.ready -eq $true`, every chapter has
zero reviewed edits and empty `pending_ids`). The job writes the reviewed files into `$stage`
and saves their SHA-256 hashes in a separate receipt inside the workspace. Create and verify the
delivery plan from that receipt before playback:

```powershell
$stageReceipt = $result.stage_receipt
$planPath = "$root/delivery-plan.json"
$ids = 1..$result.stage_files.Count
intake-agent narration-delivery-plan $stage $delivery $stageReceipt > $planPath
if ($LASTEXITCODE -ne 0) { throw 'Could not create the reviewed narration plan' }
intake-agent verify $planPath $stage $delivery @ids
if ($LASTEXITCODE -ne 0) { throw 'The reviewed narration plan failed verification' }
```

Play each staged chapter and compare its transcript with the spoken script. For chapter 001:

```powershell
& $env:INTAKE_FFMPEG -v error -i "$stage/001.wav" -f null -
Start-Process "$stage/001.wav"
Get-Content "$stage/001.transcript.txt"
```

When the audio and transcripts pass your review, apply the same plan. Compare the plan hash
displayed by `apply` with the one from `verify` and type the displayed `APPROVE` line in an
interactive terminal:

```powershell
intake-agent apply $planPath $stage $delivery "$root/delivery-run.jsonl" @ids
intake-agent receipt "$root/delivery-run.jsonl"
```

If `apply` is interrupted before completion, inspect the journal and finish recovery in an
interactive terminal:

```powershell
intake-agent recover "$root/delivery-run.jsonl" $stage $delivery
```

The Rust planner checks each staged file against the narration receipt. The plan binds the stage,
destination, and file hashes, and `apply` checks those hashes again before delivery. The delivered
folder contains the audio, blind result, queue, decisions, timing review, `job.json`, and final
transcript in the original script's exact bytes. The blind and reviewed word error rates are
recorded separately, checking case-insensitive matched tokens with punctuation ignored.

### Review a single chapter with an existing transcription receipt

Use the local Whisper Python environment from above to run `src/whisper_review.py`. In PowerShell 7, set the paths for the chapter's audio, script, original transcription receipt and, if available, a second blind transcription of the same audio. Use a fresh directory for the clips.

```powershell
New-Item -ItemType Directory -Path $clips
& $env:INTAKE_WHISPER_PYTHON src/whisper_review.py plan $receipt $script $audio --alternate $secondReceipt --clips $clips --ffmpeg $env:INTAKE_FFMPEG > $queue
& $env:INTAKE_WHISPER_PYTHON src/whisper_review.py apply $queue $decisions > $review
```

The queue groups adjacent word edits and includes audio excerpts, timed alternative words, and the baseline literal word error rate. Omit `--alternate` when a second receipt is unavailable. Each decision needs an item `id`, an `action` of `replace` or `retain`, and a short `evidence` note. Bind the decision file to the queue by setting its top-level `queue_sha256` to the SHA-256 of that file and storing the decisions in a `decisions` array. For replacements that change the number of words, provide a `timings` array with a `[start, end]` pair in seconds for each replacement word. Remove an extra word with `replace` when the expected text is empty. Use `retain` for a spoken deviation you verified and chose to keep. Leave disputed items pending.

The review output is a separate word-timed editorial result. It preserves the original receipt and reports the corrected WER and unresolved IDs. To gather more evidence for unresolved clips on CUDA, run `src/whisper_review.py rerun $queue $review $env:INTAKE_WHISPER_MODEL` using the same Python interpreter. The rerun does not receive the script. Add the reported chapter offset to any clip-relative word time before recording a new decision.

Mark a confirmed, reusable one-word replacement with `"learn": true`. Run `suggest` on a new blind receipt from the same editorial scope, then open its reference for evaluation.

```powershell
& $env:INTAKE_WHISPER_PYTHON src/whisper_review.py suggest $newReceipt $review > $suggestions
& $env:INTAKE_WHISPER_PYTHON src/whisper_review.py evaluate $newReceipt $suggestions $newScript
```

Suggestions are separate from confirmed corrections. After checking them, use `plan --suggestion $suggestions` with the same original receipt and a fresh clip directory to review the remaining differences. That queue records both scores and verifies that the suggestions only changed the recorded words.

Transcription writes a structured observation to stdout. The JSON records the MP3's full SHA-256, the weight and executable hashes, the audio sample count and the inference configuration. Match the MP3 hash to the extraction receipt to trace the transcript to the original video. Check the text and timings before making editorial decisions. Whisper timings are estimates in the decoded audio timeline.

### Review rough-note timing suggestions

```powershell
intake-agent align-notes C:/fixtures/work/rough-notes.txt C:/fixtures/work/take-transcript.json > C:/fixtures/work/alignment.json
```

Write one marker per UTF-8 line, such as `01:12.250, Opening remarks`. The clock accepts `MM:SS` or `HH:MM:SS` with an optional dot or comma fraction. The command matches the first transcript segment ending at or after each marker and proposes a padded range using segment pauses, punctuation and transitions. It clips the suggestion to the decoded audio duration. The JSON preserves line numbers, notes and transcript hashes, and segment indices. Exit code `3` means at least one marker is invalid or unmatched. Review the proposals against the source video timeline before selecting any clip ranges.

### Build and verify a selected clip package

In `selected-ranges.json`, name each desired output and supply reviewed millisecond offsets on the original video timeline. Use an existing package root distinct from the source root. One plan can include up to 32 ranges from one recording, each from 100 ms through 10 minutes.

```json
{
  "clips": [
    {"path": "opening.mp4", "start_ms": 250, "end_ms": 2850},
    {"path": "closing.mp4", "start_ms": 6100, "end_ms": 10000}
  ],
  "transcript": {
    "report": "C:/fixtures/work/take-transcript.json",
    "extraction_journal": "C:/fixtures/work/audio-run.jsonl",
    "action_id": 1
  }
}
```

```powershell
intake-agent clip-plan C:/fixtures/media take.mp4 C:/fixtures/package C:/fixtures/work/selected-ranges.json > C:/fixtures/work/package.json
intake-agent verify C:/fixtures/work/package.json C:/fixtures/media C:/fixtures/package 1 2
intake-agent apply C:/fixtures/work/package.json C:/fixtures/media C:/fixtures/package C:/fixtures/work/package-run.jsonl 1 2
intake-agent verify-package C:/fixtures/work/package-run.jsonl C:/fixtures/media C:/fixtures/package > C:/fixtures/work/package-receipt.json
```

The recipe renders H.264 with `libx264` and AAC when audio is present. A linked transcript selects the same audio stream used for extraction and records the transcript hash, extracted audio hash and extraction action. Omit the transcript object for an unlinked package. Source offsets remain separate from approximate Whisper timing. Check the resulting edit points by playing the clips. Creative edits remain in Resolve.

Only approved action IDs can publish files. The receipt includes each selected offset, output identity, byte count and SHA-256. Keep the plan, journal and linked records with the package documentation. If interrupted, use `receipt` and `recover` with the original roots. `verify-package` reopens the original and the completed clip files and checks their identities and hashes. The receipt records individual outcomes for partial runs.

## Import an existing audiobook edition

Select one legacy book directory containing `metadata.json` and `chapters/`. Create a separate destination book directory with a `chapters/` subdirectory. This workflow preserves the original book ID, chapter order, acquisition information and other metadata by copying the original files byte for byte.

```powershell
$review = & $tool library-import-plan $legacyBook $destinationBook | ConvertFrom-Json -AsHashtable
$review.already_present
$review.plan.body.actions
$review.plan | ConvertTo-Json -Depth 16 -Compress > $planPath
```

If `plan` is present, pass the saved plan to `verify` and `apply` with those same roots and the reviewed action IDs. Save the journal outside both roots. If you apply only selected actions, a fresh import review proposes the remaining files. A complete rerun reports every file under `already_present` and returns `plan: null`, so there is nothing to apply. Missing source chapters, mismatched hashes and conflicting destination files stop planning.

### Export chapters for listening

Use `library-listening-plan` with the edition directory, an existing destination folder, and the chapter numbers you want to copy. Review the nested plan and use the same `verify` and `apply` commands with the book and listening roots. The plan retains the source, chapter basenames and audio bytes. A repeat with verified destination files returns `plan: null`. An existing file with different bytes stops planning.

```powershell
& $tool library-listening-plan $destinationBook $listeningFolder 1 3
```

For the tested edition, the exported MP3 matched the legacy `sync --verify` output by basename, size and SHA-256. Legacy sync chooses an `Audiobooks/<author>/<title>` folder and writes `_audiobook-sync.json`. Here the operator chooses the listening folder and the durable application journal records the approved copy. Use the legacy card workflow for managed-device reconciliation.

### Import and find canonical text

`library-text-plan` takes an imported edition directory, the legacy per-book text directory containing `catalog.json`, and a separate destination text directory. Create the destination directory and each `assets/<text-id>/raw` subdirectory before planning. The review checks the book edition and copies the raw source, normalized text, asset record and catalog under the original IDs. It copies alternate-edition assets as well and retains the catalog's original canonical selection. Save the nested `plan` and use `verify` and `apply` with the two text directory roots. Select all proposed files to complete the catalog.

```powershell
& $tool library-text-plan $destinationBook $legacyTextCatalog $destinationTextCatalog
& $tool library-text-search $destinationBook $destinationTextCatalog 'human race'
```

Search does a case-insensitive substring match within each line of the selected canonical text for that edition. Results include the text ID, file path relative to the catalog, line number and SHA-256. This selected-book search requires no index. Legacy collection-wide FTS, chapter boundary mapping and the OCR creation workflow remain separate capabilities.

### Import and find timed chapter transcripts

`library-transcript-plan` takes the edition directory, the legacy per-book transcript directory containing `catalog.json`, and a separate destination transcript directory. Create the destination directory and each `assets/<job-id>` subdirectory named in the catalog. The plan includes all catalog assets, including inactive machine transcripts referenced by active corrections. It verifies chapter hashes, the correction's recorded parent and spoken source, and the text and timing reproduced from the reviewed spans. Apply the nested plan with the transcript roots and reviewed action IDs.

```powershell
& $tool library-transcript-plan $destinationBook $legacyTranscripts $destinationTranscripts
& $tool library-transcript-search $destinationBook $destinationTranscripts 'original recording and review'
```

Search matches terms within segments of the active transcript for each chapter. A result identifies machine or reviewed text, the parent when applicable, the cited segment and chapter audio hashes, and chapter-local playback offsets in milliseconds. `end_ms` stops at the recorded chapter duration. `stored_end_ms` retains the original segment end. Reviewed corrections retain segment timing, while word timings are available only where recorded by the machine transcript. The correction source represents spoken text. Canonical text comes from the separate text catalog.

### Optional local Kokoro speech preview

Use a dedicated Python environment. The tested runtime used 64-bit Python 3.14.5 on Windows. Install the pinned packages in `kokoro-requirements.txt` ahead of time. Select trusted local copies of the Kokoro v1.0 ONNX model and voice pack. The command uses CPU inference and the packaged phonemizer.

```powershell
& $python -m pip install -r ./kokoro-requirements.txt
$env:INTAKE_KOKORO_PYTHON = $python
$env:INTAKE_KOKORO_MODEL = $modelFile
$env:INTAKE_KOKORO_VOICES = $voicesFile
$preview = & $tool speech-preview $scratchRoot preview.txt am_michael | ConvertFrom-Json
if ($LASTEXITCODE) { throw 'Speech preview failed' }
$audio = [Convert]::FromBase64String($preview.audio.wav_base64)
$file = [IO.File]::Open($newWavFile, [IO.FileMode]::CreateNew)
try { $file.Write($audio) } finally { $file.Dispose() }
```

Use absolute paths for the runtime files and scratch directory. Select a UTF-8 text file of up to 1024 bytes beneath that directory. The CLI uses English (US) at speed 1.0 and returns a 24 kHz mono PCM WAV inside a JSON record. The record includes the input, executable, model, voice pack, worker, dependency list and output hashes. For full chapters, use the script-based job above.

## Local MCP and HTTP adapters

Build the Rust CLI and run `bun install --frozen-lockfile` from the toolkit checkout. Set these variables in the environment of the adapter process. Use absolute paths and an existing journal directory controlled by the operator.

```powershell
$env:INTAKE_BINARY = (Resolve-Path '.\target\debug\intake-agent.exe').Path
$env:INTAKE_ROOTS = '{"photos":"C:/fixtures/photos"}'
$env:INTAKE_JOURNALS = 'C:/fixtures/journals'
```

For MCP, configure a stdio server command of `bun` with arguments `--no-env-file`, the absolute path to `adapter.ts`, and `mcp`. Pass the settings above through the client's server environment. The `intake_read` tool accepts a `request` object such as `{"operation":"photo-evidence","root":"photos","files":["portrait.png"]}`. The protocol fixture uses the pinned TypeScript SDK 1.32.2.

For HTTP, supply a bearer token of at least 32 URL-safe characters in `INTAKE_HTTP_TOKEN` through the service environment and run `bun run http`. The listener binds to `127.0.0.1`. The default port is `8787`, configurable through `INTAKE_PORT`. Keep the token in the operator's credential store.

```powershell
Invoke-RestMethod -Uri 'http://127.0.0.1:8787/v1/operations' -Method Post `
  -Headers @{ Authorization = "Bearer $env:INTAKE_HTTP_TOKEN" } -ContentType 'application/json' `
  -Body '{"operation":"photo-evidence","root":"photos","files":["portrait.png"]}'
```

Both protocols return `protocol_version`, `record` and `complete`. Each request has an `operation` and the fields listed below. `root`, `source`, `destination`, `book` and `catalog` are configured root aliases. File and directory fields are relative to those roots. A `journal` is a UUID selecting `<uuid>.jsonl` in the configured journal directory. `plan` is the returned digest and body together. Some library reviews wrap it in `record.plan`.

| Operation | Request fields |
| --- | --- |
| `diagnostics` | Reports command inventory, configured aliases and execution availability |
| `scan`, `photo-evidence`, `duplicates`, `video-inventory` | `root`, `files` |
| `photo-suggest` | `root`, `file`, `model` |
| `plan` | `source`, `destination`, `directory`, `files` |
| `narration-delivery-plan` | `source`, `destination`, inline staging `receipt` |
| `organize` | `source`, `destination`, inline `decisions` |
| `caption-plan` | `root`, inline `decisions` |
| `video-archive-plan` | `source`, `destination`, `files`, returned `inventory` |
| `audio-plan` | `source`, `destination`, `file`, `output`, `track` (`auto` or integer), `quality` |
| `transcribe` | `root`, `file`, `language` |
| `align-notes` | inline `notes`, returned `transcript` |
| `clip-plan` | `source`, `destination`, `file`, inline `clips`; optional `transcript` with `report`, `extraction_journal` UUID and `action_id` |
| `library-import-plan` | `source`, `destination` |
| `library-listening-plan` | `source`, `destination`, numeric `chapters` |
| `library-text-plan`, `library-transcript-plan` | `book`, `source`, `destination` |
| `library-text-search`, `library-transcript-search` | `book`, `catalog`, `query` |
| `speech-preview` | `root`, `file`, `voice` |
| `verify` | `source`, `destination`, `plan`, numeric `ids` |
| `apply` | `source`, `destination`, `plan`, numeric `ids`, fresh `journal` |
| `receipt` | `journal`; clients polling a just-submitted run can set `allow_pending: true` for a structured pending result before its journal exists |
| `recover`, `verify-package` | `journal`, `source`, `destination` |
| `undo-plan` | `journal`, `root`, original numeric `ids` |

Set the optional engine paths in the adapter's own environment before launch. Media operations use the same engines and validation as the CLI. The interactive request budget is 120 seconds for execution, inventory, transcription, speech and vision, and 30 seconds for other operations. Use the CLI for jobs that need more time. Keep the read and execution protocols loopback or local stdio.

### Run a selected plan from an adapter

Adapters start with inspection and planning enabled. Set `INTAKE_ALLOW_EXECUTION=1` in an operator-controlled session to enable execution. MCP then advertises a separate `intake_execute` tool. HTTP accepts execution at the same authenticated loopback endpoint. On Windows the Rust worker opens a dedicated approval console. On Linux set `INTAKE_EXECUTION_ROOT` as described above and run the adapter in the foreground with a controlling terminal. For stdio MCP, use a host that keeps the adapter in the foreground terminal's process group while giving it separate protocol pipes. The Rust worker reads approval from the controlling terminal and refuses execution without one. Keep execution disabled for unattended launches.

1. Submit `photo-evidence` for the named photos. Ask the operator to resolve uncertain dates or categories. Submit the reviewed decisions through `organize` and retain the returned `record` unchanged.
2. Submit `verify` with `source`, `destination`, the returned `record` as `plan` and the exact numeric `ids` for the proposed action subset. Show the selected sources, destinations and placement policy to the operator.
3. Generate and retain a fresh journal UUID. Submit the same fields with `operation: "apply"` and the UUID as `journal`. In MCP, pass the request to `intake_execute`. The operator checks the action selection and journal, then types the displayed `APPROVE` line in the approval console or terminal. The request waits up to 120 seconds, including mutation. Set the client timeout accordingly.
4. On a timeout, cancellation or incomplete result, request `receipt` with the original UUID through `intake_read` or HTTP. Inspect `record.status` and each action's durable state. For an incomplete journal, submit `recover` with the same `journal`, `source` and `destination`. The operator reviews and types the new `RECOVER` line in the approval console or terminal. Preserve journals and stages when inspecting a conflict.

For coding agents, configure only operator-approved roots and journals. Request only explicitly selected files and preserve full plans and receipts. Treat model suggestions as proposals. Leave approval input to the operator and wait for a durable receipt before reporting a file move or sidecar as complete. Check the existing journal before retrying when the request outcome is uncertain.

External clients can run the CLI or call the HTTP route with the configured aliases and bearer token. The Windows desktop test uses redirected subprocess pipes. Verify that users can see and cancel the approval prompt before enabling execution in a new host.

The HTTP adapter was tested with Bun 1.4.4 x64 on DSM with glibc 2.36. To run it in the foreground, configure the binary, aliases, journal directory and token as described above, then run the following commands.

```sh
export INTAKE_EXECUTION_ROOT=/absolute/private-btrfs-fixture
export INTAKE_ALLOW_EXECUTION=1
bun --no-env-file adapter.ts http
```

Submit the request from another terminal on the same host. Review the selected paths and journal in the terminal running Bun. Keep its HTTP listener on loopback. A second adapter review may be refused while a Linux review is active.

To repeat the Windows desktop acceptance fixture, build with `cargo build --locked`, install with `bun install --frozen-lockfile`, set `$env:INTAKE_TEST_DESKTOP = '1'` and run `bun test` from an interactive Windows session. The fixture generates its own temporary roots and token, sends input only to its Rust approval consoles, kills a fixture worker after publication and verifies CLI, HTTP and MCP receipts. It also leaves one request unanswered for the full two-minute timeout. Run the desktop fixture and Rust execution tests sequentially because both use the Windows execution lock. Routine `bun test` skips the desktop fixture.

The Windows protocol test checks network-root refusal across CLI, HTTP and MCP. Set `INTAKE_TEST_NETWORK_ROOT` to the UNC path or mapped drive of a disposable SMB fixture containing `file-link.fixture` to repeat the live boundary check.

## License

The project source is MIT licensed. Cargo and Bun install dependencies under their own licenses. The repository contains source, dependency manifests and lockfiles. FFmpeg, Ollama, Python packages, model weights, voices and phonemizer resources are separately installed and retain their own licenses.
