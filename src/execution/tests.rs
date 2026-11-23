use super::*;
#[cfg(windows)]
use crate::windows;
use std::{
    fs,
    process::{Child, Command, Stdio},
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

static FIXTURES: Mutex<()> = Mutex::new(());
const PAYLOAD: &[u8] = b"selected intake payload";

#[test]
#[cfg_attr(target_os = "linux", ignore = "requires a private btrfs fixture root")]
fn ollama_suggestions_require_local_inventory_and_validated_fields() {
    use std::{
        io::{BufRead, BufReader},
        net::TcpListener,
    };

    let fixture = Fixture::new();
    let source = fixture.source();
    let photo = Path::new("image.png");
    let image = b"\x89PNG\r\n\x1a\nmock image bytes";
    fs::write(source.join(photo), image).unwrap();
    let digest = "a".repeat(64);
    let inventory = json!({"models":[{"model":"fixture:4b", "digest":digest}]});
    let suggestion = json!({
        "caption":"A test image", "category":null, "confidence":0.75,
        "conflicts":["Limited visible detail"]
    });
    let generated = |response: &Value| json!({"model":"fixture:4b","done":true,"done_reason":"stop","response":response.to_string()});

    let serve = |replies: Vec<(&'static str, Value, Duration)>, timeout: Duration| {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let worker = thread::spawn(move || {
            for (route, body, delay) in replies {
                let (mut connection, _) = listener.accept().unwrap();
                connection
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut input = BufReader::new(&mut connection);
                let mut line = String::new();
                input.read_line(&mut line).unwrap();
                assert_eq!(line.trim_end(), route);
                let mut bytes = 0;
                loop {
                    line.clear();
                    assert_ne!(input.read_line(&mut line).unwrap(), 0);
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(length) = line.to_ascii_lowercase().strip_prefix("content-length:")
                    {
                        bytes = length.trim().parse::<usize>().unwrap();
                    }
                }
                if bytes > 0 {
                    let mut request = vec![0; bytes];
                    input.read_exact(&mut request).unwrap();
                    let request: Value = serde_json::from_slice(&request).unwrap();
                    use base64::{Engine, engine::general_purpose::STANDARD};
                    assert_eq!(request["images"], json!([STANDARD.encode(image)]));
                }
                thread::sleep(delay);
                if delay.is_zero() {
                    write!(
                        connection,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.to_string().len(),
                        body
                    )
                    .unwrap();
                }
            }
        });
        let result =
            crate::organization::photo_suggest_at(&source, photo, "fixture:4b", port, timeout);
        // If a regression prevents all expected requests, report it without waiting on accept.
        (result, worker)
    };

    let tags = |response| ("GET /api/tags HTTP/1.1", response, Duration::ZERO);
    let generate = |response| ("POST /api/generate HTTP/1.1", response, Duration::ZERO);
    let (result, worker) = serve(
        vec![
            tags(inventory.clone()),
            generate(generated(&suggestion)),
            tags(inventory.clone()),
        ],
        Duration::from_secs(2),
    );
    let actual = serde_json::to_value(result.unwrap()).unwrap();
    worker.join().unwrap();
    assert_eq!(actual["suggestion"], suggestion);
    assert_eq!(actual["installed_model_sha256"], digest);
    assert_eq!(
        actual["asset"]["sha256"],
        format!("{:x}", Sha256::digest(image))
    );

    let mut unsafe_category = suggestion.clone();
    unsafe_category["category"] = json!("../unsafe");
    let mut remote = inventory.clone();
    remote["models"][0]["remote_host"] = json!("example.invalid");
    for (replies, expected) in [
        (vec![tags(json!({"models":[]}))], "missing from"),
        (vec![tags(remote)], "Expected a local"),
        (
            vec![
                tags(inventory.clone()),
                generate(generated(&json!({"caption":"incomplete"}))),
            ],
            "Invalid Ollama suggestion fields",
        ),
        (
            vec![
                tags(inventory.clone()),
                generate(generated(&unsafe_category)),
            ],
            "field limits",
        ),
        (
            vec![
                tags(inventory.clone()),
                generate(generated(&suggestion)),
                tags(json!({"models":[{"model":"fixture:4b","digest":"b".repeat(64)}]})),
            ],
            "changed during",
        ),
    ] {
        let (result, worker) = serve(replies, Duration::from_secs(2));
        let Err(error) = result else {
            panic!("Expected {expected}")
        };
        assert!(error.to_string().contains(expected), "{error}");
        worker.join().unwrap();
    }

    let (result, worker) = serve(
        vec![
            tags(inventory),
            (
                "POST /api/generate HTTP/1.1",
                Value::Null,
                Duration::from_millis(600),
            ),
        ],
        Duration::from_millis(200),
    );
    let Err(error) = result else {
        panic!("Expected a generation timeout")
    };
    assert!(
        error.to_string().contains("Ollama generation failed"),
        "{error}"
    );
    worker.join().unwrap();
    assert_eq!(fs::read(source.join(photo)).unwrap(), image);
    assert!(!source.join("image.png.caption.txt").exists());
    assert!(crate::organization::photo_evidence(&source, &[photo.to_owned()]).is_ok());
}

struct Fixture(PathBuf);

#[test]
fn narration_plan_binds_stage_receipt_before_and_after_planning() {
    let fixture = Fixture::new();
    let stage = fixture.source();
    let destination = fixture.destination(true);
    let receipt = fixture.0.join("stage-receipt.json");
    let transcript = stage.join("001.transcript.txt");
    let job = stage.join("job.json");
    fs::write(&transcript, b"Reviewed words").unwrap();
    fs::write(&job, b"{}").unwrap();
    let files = crate::scan(
        &stage,
        &[
            PathBuf::from("001.transcript.txt"),
            PathBuf::from("job.json"),
        ],
    )
    .unwrap()
    .assets
    .into_iter()
    .map(|asset| (asset.relative_path, asset.sha256))
    .collect::<std::collections::BTreeMap<_, _>>();
    fs::write(
        &receipt,
        serde_json::to_vec(&json!({"version":1, "stage":stage, "files":files})).unwrap(),
    )
    .unwrap();
    let plan = crate::narration_delivery_plan(&stage, &destination, &receipt).unwrap();
    assert_eq!(plan.body.actions.len(), 2);
    let path = fixture.0.join("delivery-plan.json");
    fs::write(&path, serde_json::to_vec(&plan).unwrap()).unwrap();
    verify(&path, &stage, &destination, &[1, 2]).unwrap();

    fs::write(&transcript, b"Changed after review").unwrap();
    let Err(error) = crate::narration_delivery_plan(&stage, &destination, &receipt) else {
        panic!("An edit after staging must block a new delivery plan");
    };
    assert!(error.to_string().contains("001.transcript.txt"));
    assert!(verify(&path, &stage, &destination, &[1, 2]).is_err());

    fs::write(&transcript, b"Reviewed words").unwrap();
    fs::write(&job, b"{\"changed\":true}").unwrap();
    assert!(crate::narration_delivery_plan(&stage, &destination, &receipt).is_err());
}

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("intake-test-{}-{nonce}", std::process::id()));
        fs::create_dir(&path).unwrap();
        let fixture = Self(path);
        fs::create_dir_all(fixture.source().join("sorted")).unwrap();
        fs::create_dir(fixture.0.join("output")).unwrap();
        fs::write(fixture.source_file(), PAYLOAD).unwrap();
        fs::write(fixture.source().join("b.txt"), b"second payload").unwrap();
        fixture
    }

    fn source(&self) -> PathBuf {
        self.0.join("incoming")
    }

    fn source_file(&self) -> PathBuf {
        self.source().join("a.txt")
    }

    fn destination(&self, copy: bool) -> PathBuf {
        if copy {
            self.0.join("output")
        } else {
            self.source()
        }
    }

    fn destination_file(&self, copy: bool) -> PathBuf {
        if copy {
            self.destination(true).join("a.txt")
        } else {
            self.source().join("sorted/a.txt")
        }
    }

    fn journal(&self) -> PathBuf {
        self.0.join("run.jsonl")
    }

    fn plan(&self, copy: bool) -> Plan {
        crate::plan(
            &self.source(),
            &self.destination(copy),
            Path::new(if copy { "." } else { "sorted" }),
            &[PathBuf::from("a.txt"), PathBuf::from("b.txt")],
        )
        .unwrap()
    }

    fn start(
        &self,
        plan: Plan,
        copy: bool,
        selection: &[u32],
        observe: impl FnMut(Boundary),
    ) -> io::Result<(Value, bool)> {
        let selection = validate_plan(&plan, selection)?;
        start(
            plan,
            selection,
            &self.source(),
            &self.destination(copy),
            &self.journal(),
            observe,
        )
    }

    fn resume(&self, copy: bool) -> (Value, bool) {
        let journal = Journal::open(&self.journal(), false).unwrap();
        let reviewed = journal.run.clone();
        drop(journal);
        resume(
            &self.journal(),
            &self.source(),
            &self.destination(copy),
            &reviewed,
        )
        .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let Ok(path) = self.0.canonicalize() else {
            return;
        };
        let Ok(temp) = std::env::temp_dir().canonicalize() else {
            return;
        };
        if path.parent() == Some(&temp)
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("intake-test-"))
        {
            let _ = fs::remove_dir_all(path);
        }
    }
}

#[test]
#[cfg_attr(target_os = "linux", ignore = "requires a private btrfs fixture root")]
fn checkpoint_preserves_complete_records_and_repairs_only_torn_tail() {
    let fixture = Fixture::new();
    let root = checked_root(&fixture.source()).unwrap();
    let checkpoint = fixture.0.join("probe.jsonl");
    let header = b"{\"video_checkpoint\":1}\n";
    fs::write(&checkpoint, &header[..9]).unwrap();
    let (mut file, records) = open_checkpoint::<u32>(&checkpoint, &root, header).unwrap();
    assert!(records.is_empty());
    assert!(open_checkpoint::<u32>(&checkpoint, &root, header).is_err());
    append_checkpoint(&mut file, &42).unwrap();
    file.write_all(b"1").unwrap();
    file.sync_all().unwrap();
    drop(file);

    let (mut file, records) = open_checkpoint::<u32>(&checkpoint, &root, header).unwrap();
    assert_eq!(records, vec![42]);
    append_checkpoint(&mut file, &7).unwrap();
    drop(file);
    assert_eq!(
        fs::read(&checkpoint).unwrap(),
        [header.as_slice(), b"42\n7\n"].concat()
    );

    let corrupted = [header.as_slice(), b"42\nx\npartial"].concat();
    fs::write(&checkpoint, &corrupted).unwrap();
    assert!(open_checkpoint::<u32>(&checkpoint, &root, header).is_err());
    assert_eq!(fs::read(&checkpoint).unwrap(), corrupted);
}

#[test]
#[cfg_attr(target_os = "linux", ignore = "requires a private btrfs fixture root")]
fn selected_moves_and_verified_copies() {
    let _test = FIXTURES.lock().unwrap_or_else(|e| e.into_inner());
    for copy in [false, true] {
        let f = Fixture::new();
        let plan = f.plan(copy);
        let expected = plan.body.actions[0].source.file_id;
        let (receipt, complete) = f.start(plan, copy, &[1], |_| {}).unwrap();
        assert!(complete, "{receipt}");
        let method = if copy { "copy_retain" } else { "move" };
        assert_eq!(
            receipt["actions"][0]["last_durable_state"]["method"],
            method
        );
        assert_eq!(receipt["actions"].as_array().unwrap().len(), 1);
        assert_eq!(fs::read(f.destination_file(copy)).unwrap(), PAYLOAD);
        assert_eq!(f.source_file().exists(), copy);
        assert!(f.source().join("b.txt").is_file());
        let output = File::open(f.destination_file(copy)).unwrap();
        assert_eq!(
            Some(platform::identity(&output).unwrap()) == expected,
            !copy
        );
        assert_eq!(super::receipt(&f.journal()).unwrap(), receipt);
        assert_eq!(f.resume(copy).0, receipt);
    }
}

#[test]
#[cfg_attr(target_os = "linux", ignore = "requires a private btrfs fixture root")]
fn stale_unsafe_and_conflicting_inputs() {
    let _test = FIXTURES.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let plan = f.plan(false);
    let mut edited = plan.clone();
    edited.body.actions[0].source.bytes += 1;
    assert!(validate_plan(&edited, &[1]).is_err());
    edited.body.actions[0].destination = PathBuf::from("../a.txt");
    edited.sha256 = plan_digest(&edited.body).unwrap();
    assert!(validate_plan(&edited, &[1]).is_err());
    assert!(validate_plan(&plan, &[1, 1]).is_err());
    assert!(validate_plan(&plan, &[3]).is_err());
    assert!(Roots::bind(&plan, &f.source(), &f.destination(true)).is_err());
    assert!(journal_path(&f.source().join("run.jsonl"), Some(&plan)).is_err());

    let target = f.source().join("sorted");
    #[cfg(windows)]
    {
        let pinned = PinnedDirectory::open(&checked_root(&target).unwrap()).unwrap();
        assert!(
            fs::rename(&target, f.source().join("moved")).is_err(),
            "directory pin failed"
        );
        drop(pinned);
    }

    fs::write(f.source_file(), vec![b'x'; PAYLOAD.len()]).unwrap();
    let error = f.start(plan, false, &[1], |_| {}).unwrap_err();
    assert!(error.to_string().contains("bytes changed"));
    assert!(!f.journal().exists());
    assert!(!f.destination_file(false).exists());
    fs::write(f.source_file(), PAYLOAD).unwrap();

    let plan = f.plan(false);
    fs::rename(f.source_file(), f.0.join("saved.txt")).unwrap();
    fs::write(f.source_file(), PAYLOAD).unwrap();
    let error = f.start(plan, false, &[1], |_| {}).unwrap_err();
    assert!(error.to_string().contains("file identity"));

    let plan = f.plan(false);
    fs::write(f.destination_file(false), b"do not replace").unwrap();
    assert!(f.start(plan, false, &[1], |_| {}).is_err());
    assert_eq!(
        fs::read(f.destination_file(false)).unwrap(),
        b"do not replace"
    );
    fs::remove_file(f.destination_file(false)).unwrap();

    let plan = f.plan(false);
    fs::rename(&target, f.source().join("moved")).unwrap();
    assert!(f.start(plan.clone(), false, &[1], |_| {}).is_err());
    fs::create_dir(&target).unwrap();
    assert!(f.start(plan.clone(), false, &[1], |_| {}).is_err());
    fs::remove_dir(&target).unwrap();

    #[cfg(windows)]
    let junction = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command",
            "New-Item -ErrorAction Stop -ItemType Junction -Path $env:INTAKE_TEST_LINK -Target $env:INTAKE_TEST_TARGET | Out-Null"])
        .env("INTAKE_TEST_LINK", &target)
        .env("INTAKE_TEST_TARGET", f.destination(true))
        .status().unwrap();
    #[cfg(windows)]
    assert!(junction.success(), "fixture junction could not be created");
    #[cfg(target_os = "linux")]
    std::os::unix::fs::symlink(f.destination(true), &target).unwrap();
    let refused = f.start(plan, false, &[1], |_| {}).unwrap_err();
    #[cfg(windows)]
    assert!(refused.to_string().contains("reparse"));
    #[cfg(target_os = "linux")]
    assert!(refused.to_string().contains("without links"));
    #[cfg(windows)]
    fs::remove_dir(&target).unwrap();
    #[cfg(target_os = "linux")]
    fs::remove_file(&target).unwrap();
    assert!(!f.journal().exists());
    assert_eq!(fs::read(f.source_file()).unwrap(), PAYLOAD);
    assert!(!f.destination_file(true).exists());
}

#[test]
#[cfg_attr(target_os = "linux", ignore = "requires a private btrfs fixture root")]
fn late_collision_reports_partial_and_resumes() {
    let _test = FIXTURES.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let second = f.destination(true).join("b.txt");
    let (receipt, completed) = f
        .start(f.plan(true), true, &[1, 2], |event| {
            if event == Boundary::AfterReceipt {
                fs::write(&second, b"external collision").unwrap();
            }
        })
        .unwrap();
    assert!(!completed);
    assert_eq!(receipt["actions"][0]["status"], "completed");
    assert_eq!(receipt["actions"][1]["status"], "failed");
    assert_eq!(fs::read(&second).unwrap(), b"external collision");
    assert_eq!(receipt, super::receipt(&f.journal()).unwrap());
    fs::remove_file(&second).unwrap();
    let (receipt, completed) = f.resume(true);
    assert!(completed, "{receipt}");
    assert_eq!(fs::read(second).unwrap(), b"second payload");
}

#[test]
#[cfg_attr(target_os = "linux", ignore = "requires a private btrfs fixture root")]
fn publication_never_overwrites_a_late_collision() {
    let _test = FIXTURES.lock().unwrap_or_else(|e| e.into_inner());
    for copy in [false, true] {
        let f = Fixture::new();
        let target = f.destination_file(copy);
        let (receipt, completed) = f
            .start(f.plan(copy), copy, &[1], |event| {
                if event == Boundary::BeforePublication {
                    fs::write(&target, b"late collision").unwrap();
                }
            })
            .unwrap();
        assert!(!completed, "{receipt}");
        assert_eq!(receipt["actions"][0]["status"], "failed");
        assert_eq!(fs::read(&target).unwrap(), b"late collision");
        assert_eq!(fs::read(f.source_file()).unwrap(), PAYLOAD);
        fs::remove_file(&target).unwrap();
        let (receipt, completed) = f.resume(copy);
        assert!(completed, "{receipt}");
        assert_eq!(fs::read(target).unwrap(), PAYLOAD);
        assert_eq!(f.source_file().exists(), copy);
    }
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires a private btrfs fixture root"]
fn linux_execution_refuses_shared_directories_hard_links_and_case_collisions() {
    use std::os::unix::fs::PermissionsExt;

    let _test = FIXTURES.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let plan = f.plan(false);
    let folder = f.source().join("sorted");
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o770)).unwrap();
    let error = f.start(plan.clone(), false, &[1], |_| {}).unwrap_err();
    assert!(error.to_string().contains("private"));
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o700)).unwrap();

    let link = f.0.join("alias.txt");
    fs::hard_link(f.source_file(), &link).unwrap();
    let error = f.start(plan.clone(), false, &[1], |_| {}).unwrap_err();
    assert!(error.to_string().contains("hard links"));
    fs::remove_file(link).unwrap();
    assert!(!f.journal().exists());

    let target = folder.join("A.TXT");
    let (receipt, complete) = f
        .start(plan, false, &[1], |event| {
            if event == Boundary::BeforePublication {
                fs::write(&target, b"case collision").unwrap();
            }
        })
        .unwrap();
    assert!(!complete);
    assert_eq!(receipt["actions"][0]["status"], "failed");
    assert_eq!(fs::read(target).unwrap(), b"case collision");
    assert_eq!(fs::read(f.source_file()).unwrap(), PAYLOAD);
    assert!(!f.destination_file(false).exists());
}

#[test]
#[cfg_attr(target_os = "linux", ignore = "requires a private btrfs fixture root")]
fn competing_process_cannot_start_a_run() {
    let _test = FIXTURES.lock().unwrap_or_else(|e| e.into_inner());
    let active = Fixture::new();
    let competing = Fixture::new();
    let worker = Worker::pause(&active, false, Boundary::BeforePublication);
    let plan = competing.plan(true);
    let refused = competing
        .start(plan.clone(), true, &[1], |_| {})
        .unwrap_err();
    assert_eq!(refused.kind(), io::ErrorKind::WouldBlock);
    assert!(!competing.journal().exists());
    assert!(!competing.destination_file(true).exists());
    worker.interrupt();
    let (receipt, completed) = competing.start(plan, true, &[1], |_| {}).unwrap();
    assert!(completed, "{receipt}");
    let (receipt, completed) = active.resume(false);
    assert!(completed, "{receipt}");
}

#[test]
#[cfg_attr(target_os = "linux", ignore = "requires a private btrfs fixture root")]
fn undo_refuses_conflicts_and_restores_only_the_approved_move() {
    let _test = FIXTURES.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    assert!(f.start(f.plan(false), false, &[1], |_| {}).unwrap().1);
    let plan = undo_plan(&f.journal(), &f.source(), &[1]).unwrap();
    assert!(undo_plan(&f.journal(), &f.source(), &[2]).is_err());

    fs::write(f.source_file(), b"occupying original name").unwrap();
    assert!(undo_plan(&f.journal(), &f.source(), &[1]).is_err());
    let rejected = start(
        plan.clone(),
        vec![1],
        &f.source(),
        &f.source(),
        &f.0.join("undo.jsonl"),
        |_| {},
    );
    assert!(rejected.is_err());
    assert_eq!(
        fs::read(f.source_file()).unwrap(),
        b"occupying original name"
    );
    fs::remove_file(f.source_file()).unwrap();

    let moved = f.destination_file(false);
    let changed = vec![b'x'; PAYLOAD.len()];
    fs::write(&moved, &changed).unwrap();
    assert!(undo_plan(&f.journal(), &f.source(), &[1]).is_err());
    fs::write(&moved, PAYLOAD).unwrap();
    assert!(
        start(
            plan,
            vec![1],
            &f.source(),
            &f.source(),
            &f.0.join("undo.jsonl"),
            |_| {}
        )
        .unwrap()
        .1
    );
    assert_eq!(fs::read(f.source_file()).unwrap(), PAYLOAD);
    assert!(!moved.exists());
    assert_eq!(
        fs::read(f.source().join("b.txt")).unwrap(),
        b"second payload"
    );
    assert!(undo_plan(&f.journal(), &f.source(), &[1]).is_err());

    let f = Fixture::new();
    assert!(f.start(f.plan(true), true, &[1], |_| {}).unwrap().1);
    assert!(undo_plan(&f.journal(), &f.source(), &[1]).is_err());
    assert!(f.source_file().exists());
    assert!(f.destination_file(true).exists());
}

#[test]
#[cfg_attr(target_os = "linux", ignore = "requires a private btrfs fixture root")]
fn organize_uses_validated_dates_and_categories_with_full_hash_duplicate_review() {
    let _test = FIXTURES.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let root = f.source();
    fs::create_dir_all(root.join("photos/2024/02/29")).unwrap();
    fs::create_dir_all(root.join("2026/11/18")).unwrap();
    fs::write(root.join("copy.txt"), PAYLOAD).unwrap();
    fs::write(root.join("different.txt"), vec![b'x'; PAYLOAD.len()]).unwrap();
    let layout = f.0.join("decisions.json");
    fs::write(&layout, r#"[{"file":"a.txt","date":"2024-02-29","category":"photos"},{"file":"b.txt","date":"2026-11-18"}]"#).unwrap();
    let plan = crate::organization::organize(&root, &root, &layout).unwrap();
    assert_eq!(
        plan.body.actions[0].destination,
        Path::new("photos/2024/02/29/a.txt")
    );
    assert_eq!(
        plan.body.actions[1].destination,
        Path::new("2026/11/18/b.txt")
    );
    assert!(
        plan.body
            .actions
            .iter()
            .all(|action| action.destination_parent_id.is_some())
    );
    let selected = ["a.txt", "copy.txt", "different.txt"].map(PathBuf::from);
    let report =
        serde_json::to_value(crate::organization::duplicates(&root, &selected).unwrap()).unwrap();
    assert_eq!(report["groups"].as_array().unwrap().len(), 1);
    assert_eq!(report["groups"][0].as_array().unwrap().len(), 2);
    assert_eq!(report["groups"][0][1]["relative_path"], "copy.txt");
    for invalid in [
        r#"[{"file":"a.txt","date":"2023-02-29"}]"#,
        r#"[{"file":"a.txt","date":"2024-02-29","category":"../photos"}]"#,
        r#"[{"file":"a.txt","date":"2024-02-29","unexpected":1}]"#,
        r#"[{"file":"a.txt","date":"2024-03-01","category":"photos"}]"#,
        r#"[{"file":"a.txt","date":"2024-02-29","category":"photos","name":"nested/a.txt"}]"#,
        r#"[{"file":"a.txt","date":"2024-02-29","category":"photos","name":"a.exe"}]"#,
        r#"[{"file":"a.txt","date":"2024-02-29","category":"photos","expected_sha256":"stale"}]"#,
    ] {
        fs::write(&layout, invalid).unwrap();
        assert!(
            crate::organization::organize(&root, &root, &layout).is_err(),
            "{invalid}"
        );
    }
    assert!(f.start(plan, false, &[1], |_| {}).unwrap().1);
    assert_eq!(
        fs::read(root.join("photos/2024/02/29/a.txt")).unwrap(),
        PAYLOAD
    );
    assert!(root.join("b.txt").is_file());
    assert!(!root.join("2026/11/18/b.txt").exists());
}

#[test]
#[cfg_attr(target_os = "linux", ignore = "requires a private btrfs fixture root")]
fn reviewed_rename_preserves_bytes_and_can_be_reversed() {
    let _test = FIXTURES.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let root = f.source();
    fs::create_dir_all(root.join("2024/02/29")).unwrap();
    let decisions = f.0.join("decisions.json");
    let hash = f.plan(false).body.actions[0].source.sha256.clone();
    fs::write(&decisions, format!(
        r#"[{{"file":"a.txt","date":"2024-02-29","name":"reviewed.TXT","expected_sha256":"{hash}"}}]"#
    )).unwrap();
    let plan = crate::organization::organize(&root, &root, &decisions).unwrap();
    assert!(f.start(plan, false, &[1], |_| {}).unwrap().1);
    let moved = root.join("2024/02/29/reviewed.TXT");
    assert_eq!(fs::read(&moved).unwrap(), PAYLOAD);
    let reverse = undo_plan(&f.journal(), &root, &[1]).unwrap();
    assert!(
        start(
            reverse,
            vec![1],
            &root,
            &root,
            &f.0.join("undo.jsonl"),
            |_| {}
        )
        .unwrap()
        .1
    );
    assert!(!moved.exists());
    assert_eq!(fs::read(f.source_file()).unwrap(), PAYLOAD);
}

#[test]
#[cfg_attr(target_os = "linux", ignore = "requires a private btrfs fixture root")]
fn photo_capture_dates_preserve_raw_evidence_and_flag_conflicts() {
    let f = Fixture::new();
    let root = f.source();
    let original = exif::Field {
        tag: exif::Tag::DateTimeOriginal,
        ifd_num: exif::In::PRIMARY,
        value: exif::Value::Ascii(vec![b"2024:02:29 20:30:59".to_vec()]),
    };
    let digitized = exif::Field {
        tag: exif::Tag::DateTimeDigitized,
        ifd_num: exif::In::PRIMARY,
        value: exif::Value::Ascii(vec![b"2024:03:01 20:30:59".to_vec()]),
    };
    let mut writer = exif::experimental::Writer::new();
    writer.push_field(&original);
    writer
        .write(&mut File::create(root.join("capture.tif")).unwrap(), false)
        .unwrap();
    writer.push_field(&digitized);
    writer
        .write(&mut File::create(root.join("conflict.tif")).unwrap(), false)
        .unwrap();
    let selected = ["capture.tif", "conflict.tif", "a.txt"].map(PathBuf::from);
    let report =
        serde_json::to_value(crate::organization::photo_evidence(&root, &selected).unwrap())
            .unwrap();
    let photos = report["photos"].as_array().unwrap();
    assert_eq!(photos[0]["evidence"]["filing_date"], "2024-02-29");
    assert_eq!(
        photos[1]["evidence"]["dates"][1]["raw"],
        "2024:03:01 20:30:59"
    );
    assert_eq!(photos[1]["evidence"]["capture_date_conflict"], true);
    assert!(photos[1]["evidence"]["filing_date"].is_null());
    assert_eq!(photos[2]["evidence"]["exif_status"], "unreadable");
    for (photo, asset) in photos
        .iter()
        .zip(crate::scan(&root, &selected).unwrap().assets)
    {
        assert_eq!(photo["asset"]["sha256"], asset.sha256);
    }
}

struct Worker(Child);

impl Worker {
    fn pause(fixture: &Fixture, copy: bool, boundary: Boundary) -> Self {
        Self::pause_inner(fixture, copy, false, boundary)
    }

    fn pause_inner(fixture: &Fixture, copy: bool, caption: bool, boundary: Boundary) -> Self {
        let marker = fixture.0.join("paused");
        let worker = Self(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "execution::tests::interruption_worker",
                ])
                .env("INTAKE_TEST_FIXTURE", &fixture.0)
                .env("INTAKE_TEST_BOUNDARY", format!("{boundary:?}"))
                .env("INTAKE_TEST_COPY", if copy { "yes" } else { "no" })
                .env("INTAKE_TEST_CAPTION", if caption { "yes" } else { "no" })
                .stdin(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let start = Instant::now();
        while !marker.is_file() && start.elapsed() < Duration::from_secs(10) {
            thread::sleep(Duration::from_millis(15));
        }
        assert!(marker.is_file(), "worker did not pause at {boundary:?}");
        worker
    }

    fn interrupt(mut self) {
        self.0.kill().unwrap();
        assert!(!self.0.wait().unwrap().success());
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "child process fixture entry point"]
fn interruption_worker() {
    let fixture = std::env::var_os("INTAKE_TEST_FIXTURE").expect("test runner fixture");
    let fixture = std::mem::ManuallyDrop::new(Fixture(fixture.into()));
    let checkpoint = std::env::var("INTAKE_TEST_BOUNDARY").unwrap();
    let copy = std::env::var("INTAKE_TEST_COPY").unwrap() == "yes";
    let caption = std::env::var("INTAKE_TEST_CAPTION").is_ok_and(|v| v == "yes");
    let plan = if let Some(path) = std::env::var_os("INTAKE_TEST_PLAN") {
        load_plan(Path::new(&path)).unwrap()
    } else if caption {
        caption_fixture(&fixture)
    } else {
        fixture.plan(copy)
    };
    let (receipt, _) = fixture
        .start(plan, copy, &[1], |boundary| {
            if checkpoint == format!("{boundary:?}") {
                fs::write(fixture.0.join("paused"), "ready").unwrap();
                loop {
                    thread::park();
                }
            }
        })
        .unwrap();
    panic!("worker should have reached {checkpoint}: {receipt}");
}

#[cfg(windows)]
#[test]
#[ignore = "child process fixture entry point"]
fn approval_console_worker() {
    use std::{fs::OpenOptions, os::windows::io::AsRawHandle};
    use windows_sys::Win32::System::Console::{
        AttachConsole, FreeConsole, GetConsoleTitleW, INPUT_RECORD, INPUT_RECORD_0, KEY_EVENT,
        KEY_EVENT_RECORD, KEY_EVENT_RECORD_0, WriteConsoleInputW,
    };

    let pid = std::env::var("INTAKE_TEST_CONSOLE_PID")
        .unwrap()
        .parse::<u32>()
        .unwrap();
    assert_ne!(pid, u32::MAX, "fixture must target its own child PID");
    assert_ne!(unsafe { FreeConsole() }, 0);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut title = [0u16; 128];
    loop {
        if unsafe { AttachConsole(pid) } != 0 {
            let length = unsafe { GetConsoleTitleW(title.as_mut_ptr(), title.len() as u32) };
            if String::from_utf16_lossy(&title[..length as usize])
                == windows::APPROVAL_CONSOLE_TITLE
            {
                break;
            }
            assert_ne!(unsafe { FreeConsole() }, 0);
        }
        assert!(
            Instant::now() < deadline,
            "child has no dedicated approval console"
        );
        thread::sleep(Duration::from_millis(25));
    }

    if let Some(answer) = std::env::var("INTAKE_TEST_CONSOLE_ANSWER")
        .ok()
        .filter(|text| !text.is_empty())
    {
        let input = OpenOptions::new().write(true).open("CONIN$").unwrap();
        let keys: Vec<_> = answer
            .encode_utf16()
            .chain([13])
            .map(|character| INPUT_RECORD {
                EventType: KEY_EVENT as u16,
                Event: INPUT_RECORD_0 {
                    KeyEvent: KEY_EVENT_RECORD {
                        bKeyDown: 1,
                        wRepeatCount: 1,
                        wVirtualKeyCode: if character == 13 { 13 } else { 0 },
                        uChar: KEY_EVENT_RECORD_0 {
                            UnicodeChar: character,
                        },
                        ..KEY_EVENT_RECORD::default()
                    },
                },
            })
            .collect();
        let mut written = 0;
        let success = unsafe {
            WriteConsoleInputW(
                input.as_raw_handle(),
                keys.as_ptr(),
                keys.len() as u32,
                &mut written,
            )
        };
        assert_ne!(success, 0, "{}", io::Error::last_os_error());
        assert_eq!(written as usize, keys.len());
    }
    assert_ne!(unsafe { FreeConsole() }, 0);
}

fn caption_fixture(f: &Fixture) -> Plan {
    let path = f.0.join("captions.json");
    fs::write(
        &path,
        r#"[{"file":"a.txt","caption":"Portrait — evening"},
                         {"file":"b.txt","caption":"Unselected"}]"#,
    )
    .unwrap();
    crate::organization::caption_plan(&f.source(), &path).unwrap()
}

#[test]
#[cfg_attr(target_os = "linux", ignore = "requires a private btrfs fixture root")]
fn caption_publication_reuses_durable_stage_and_refuses_foreign_outputs() {
    let _test = FIXTURES.lock().unwrap_or_else(|e| e.into_inner());
    for boundary in [
        Boundary::StageCreated,
        Boundary::BeforePublication,
        Boundary::AfterPublication,
        Boundary::BeforeReceipt,
    ] {
        let f = Fixture::new();
        Worker::pause_inner(&f, false, true, boundary).interrupt();
        let caption = f.source().join("a.txt.caption.txt");
        let published = matches!(
            boundary,
            Boundary::AfterPublication | Boundary::BeforeReceipt
        );
        assert_eq!(caption.exists(), published);
        assert_eq!(fs::read(f.source_file()).unwrap(), PAYLOAD);
        let (receipt, done) = f.resume(false);
        assert!(done, "{receipt}");
        assert_eq!(
            receipt["actions"][0]["last_durable_state"]["method"],
            "caption"
        );
        assert_eq!(
            receipt["actions"][0]["last_durable_state"]["reconciled"],
            published
        );
        assert_eq!(fs::read_to_string(&caption).unwrap(), "Portrait — evening");
        assert!(!f.source().join("b.txt.caption.txt").exists());
        assert_eq!(fs::read(f.source_file()).unwrap(), PAYLOAD);
        assert!(undo_plan(&f.journal(), &f.source(), &[1]).is_err());
    }

    let f = Fixture::new();
    let caption = f.source().join("a.txt.caption.txt");
    let plan = caption_fixture(&f);
    let (receipt, done) = f
        .start(plan, false, &[1], |boundary| {
            if boundary == Boundary::BeforePublication {
                fs::write(&caption, b"existing caption").unwrap();
            }
        })
        .unwrap();
    assert!(!done, "{receipt}");
    assert_eq!(fs::read(&caption).unwrap(), b"existing caption");
    assert!(!f.resume(false).1);
    assert_eq!(fs::read(&caption).unwrap(), b"existing caption");
    assert!(crate::organization::caption_plan(&f.source(), &f.0.join("captions.json")).is_err());
    assert_eq!(fs::read(f.source_file()).unwrap(), PAYLOAD);
}

#[test]
#[cfg_attr(target_os = "linux", ignore = "requires a private btrfs fixture root")]
fn caption_plans_bind_source_bytes_and_derived_sidecar_path() {
    let _test = FIXTURES.lock().unwrap_or_else(|e| e.into_inner());
    let f = Fixture::new();
    let mut edited = caption_fixture(&f);
    edited.body.actions[0].destination = PathBuf::from("arbitrary.txt");
    edited.sha256 = plan_digest(&edited.body).unwrap();
    assert!(f.start(edited, false, &[1], |_| {}).is_err());
    assert!(!f.journal().exists());
    let decision = f.0.join("captions.json");
    for invalid in [
        r#"[{"file":"a.txt","caption":"   "}]"#,
        r#"[{"file":"a.txt","caption":"contains\u001b escape"}]"#,
        r#"[{"file":"a.txt","caption":"reviewed", "expected_sha256":"stale"}]"#,
    ] {
        fs::write(&decision, invalid).unwrap();
        assert!(crate::organization::caption_plan(&f.source(), &decision).is_err());
    }
    let stale = caption_fixture(&f);
    fs::write(f.source_file(), "changed").unwrap();
    assert!(f.start(stale, false, &[1], |_| {}).is_err());
    assert!(!f.source().join("a.txt.caption.txt").exists());
}

#[test]
#[cfg_attr(target_os = "linux", ignore = "requires a private btrfs fixture root")]
fn killed_process_reconciles_publication_and_receipts() {
    let _test = FIXTURES.lock().unwrap_or_else(|e| e.into_inner());
    for copy in [false, true] {
        for phase in [
            Boundary::JournalReady,
            Boundary::BeforePublication,
            Boundary::AfterPublication,
            Boundary::BeforeReceipt,
            Boundary::AfterReceipt,
        ] {
            let f = Fixture::new();
            let worker = Worker::pause(&f, copy, phase);
            assert_eq!(
                RunGuard::acquire().err().unwrap().kind(),
                io::ErrorKind::WouldBlock
            );
            worker.interrupt();
            let published = matches!(
                phase,
                Boundary::AfterPublication | Boundary::BeforeReceipt | Boundary::AfterReceipt
            );
            assert_eq!(f.destination_file(copy).exists(), published);
            assert_eq!(f.source_file().exists(), copy || !published);
            let before = super::receipt(&f.journal()).unwrap();
            assert_eq!(
                before["status"] == "completed",
                phase == Boundary::AfterReceipt
            );
            let (after, complete) = f.resume(copy);
            assert!(complete, "{phase:?} {copy}: {after}");
            let reconciled = &after["actions"][0]["last_durable_state"]["reconciled"];
            assert_eq!(
                reconciled,
                matches!(phase, Boundary::AfterPublication | Boundary::BeforeReceipt)
            );
            assert_eq!(fs::read(f.destination_file(copy)).unwrap(), PAYLOAD);
            assert_eq!(f.source_file().exists(), copy);
            assert_eq!(f.resume(copy).0, after);
        }
    }
    let f = Fixture::new();
    Worker::pause(&f, true, Boundary::StageCreated).interrupt();
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(f.journal())
        .unwrap();
    file.write_all(b"{\"event\":\"transition\",\"id\":")
        .unwrap();
    file.sync_all().unwrap();
    drop(file);
    assert_eq!(super::receipt(&f.journal()).unwrap()["torn_tail"], true);
    let (receipt, complete) = f.resume(true);
    assert!(complete, "{receipt}");
    assert_eq!(receipt["torn_tail"], false);
    assert_eq!(fs::read(f.source_file()).unwrap(), PAYLOAD);
}

#[test]
#[cfg_attr(target_os = "linux", ignore = "requires a private btrfs fixture root")]
fn recovery_refuses_lookalike_destination() {
    let _test = FIXTURES.lock().unwrap_or_else(|e| e.into_inner());
    for copy in [false, true] {
        let f = Fixture::new();
        Worker::pause(&f, copy, Boundary::AfterPublication).interrupt();
        let destination = f.destination_file(copy);
        let preserved = f.0.join("original-output.txt");
        fs::rename(&destination, &preserved).unwrap();
        fs::write(&destination, PAYLOAD).unwrap();
        let (receipt, complete) = f.resume(copy);
        assert!(!complete, "{receipt}");
        assert_eq!(receipt["actions"][0]["status"], "failed");
        assert!(
            receipt["actions"][0]["error"]
                .as_str()
                .unwrap()
                .contains("file identity")
        );
        assert_eq!(fs::read(preserved).unwrap(), PAYLOAD);
        assert_eq!(fs::read(destination).unwrap(), PAYLOAD);
        assert_eq!(f.source_file().exists(), copy);
    }
}
