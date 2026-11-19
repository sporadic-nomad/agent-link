//! Exercise the public bindings with real foreground PTYs and isolated on-disk records.
#![cfg(feature = "turns")]

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
#[test]
fn isolated_sources_require_native_launch_evidence() {
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(
        agent_link::agent::turns::IsolatedTranscriptRoot::new(dir.path()),
        Err(agent_link::Error::Unsupported(_))
    ));
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod native {
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use agent_link::agent::profiles::{CodexProfile, CursorProfile, PiProfile};
    use agent_link::agent::turns::{
        IsolatedTranscriptRoot, TranscriptReceiver, TranscriptSource as Source, TranscriptTurn,
        TurnGap, TurnOptions, TurnStatus,
    };
    use agent_link::agent::{AgentProfile, AgentSpec, AgentTerminal};
    use agent_link::{Error, PtySize, SpawnSpec, TerminalIdentity};
    use serde_json::{Value, json};

    const CODEX: &str = include_str!("fixtures/transcripts/codex-0.154.0/turns.jsonl");
    const PI: &str = include_str!("fixtures/transcripts/pi-1.5.2/turns.jsonl");
    const CURSOR: [&str; 3] = [
        include_str!("fixtures/transcripts/cursor-2026.11.13-eacade1/snapshot-0.jsonl"),
        include_str!("fixtures/transcripts/cursor-2026.11.13-eacade1/snapshot-1.jsonl"),
        include_str!("fixtures/transcripts/cursor-2026.11.13-eacade1/snapshot-2.jsonl"),
    ];
    const CID: &str = "22222222-2222-4222-8222-222222222222";

    struct Setup {
        _scratch: tempfile::TempDir,
        cwd: PathBuf,
    }

    impl Setup {
        fn new() -> Self {
            let scratch = tempfile::tempdir().unwrap();
            let cwd = scratch.path().canonicalize().unwrap();
            symlink("/bin/cat", cwd.join("codex")).unwrap();
            fs::create_dir(cwd.join("cursor-package")).unwrap();
            symlink("/bin/bash", cwd.join("cursor-package/node")).unwrap();
            fs::write(
                cwd.join("cursor-package/index.js"),
                "while read -r line; do :; done\n",
            )
            .unwrap();
            symlink("/bin/bash", cwd.join("node")).unwrap();
            let pi = cwd.join("pi-coding-agent/dist/bundle");
            fs::create_dir_all(&pi).unwrap();
            fs::write(pi.join("cli.js"), "while read -r line; do :; done\n").unwrap();
            Self {
                _scratch: scratch,
                cwd,
            }
        }

        fn spec(&self, source: Source) -> (SpawnSpec, AgentSpec) {
            let (prog, profile): (_, Arc<dyn AgentProfile>) = match source {
                Source::Codex => ("codex", Arc::new(CodexProfile)),
                Source::Pi => ("node", Arc::new(PiProfile::default())),
                _ => ("cursor-package/node", Arc::new(CursorProfile)),
            };
            let args = match source {
                Source::Pi => Some(self.cwd.join("pi-coding-agent/dist/bundle/cli.js")),
                Source::Codex => None,
                _ => Some(self.cwd.join("cursor-package/index.js")),
            };
            let spec = SpawnSpec::direct(
                self.cwd.join(prog),
                args,
                &self.cwd,
                PtySize { rows: 24, cols: 80 },
            );
            (spec, AgentSpec::new(profile))
        }

        fn lease(&self, name: &str) -> (PathBuf, IsolatedTranscriptRoot) {
            let dir = self.cwd.join(name);
            fs::create_dir(&dir).unwrap();
            let lease = IsolatedTranscriptRoot::new(&dir).unwrap();
            (dir, lease)
        }

        fn attach(
            &self,
            source: Source,
            lease: &IsolatedTranscriptRoot,
            options: TurnOptions,
        ) -> (AgentTerminal, TranscriptReceiver) {
            let (spec, agent) = self.spec(source);
            let (term, _events) =
                AgentTerminal::spawn_with_transcript_turns(spec, agent, source, lease).unwrap();
            let receiver = term.attach_transcript_turns(options).unwrap();
            (term, receiver)
        }

        fn cursor_path(&self, root: &Path) -> PathBuf {
            let slug = self
                .cwd
                .to_str()
                .unwrap()
                .split(|ch: char| !ch.is_ascii_alphanumeric())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("-");
            root.join("projects")
                .join(slug)
                .join("agent-transcripts")
                .join(CID)
                .join(format!("{CID}.jsonl"))
        }

        fn codex_header(&self, root: &Path) -> PathBuf {
            let mut header: Value = serde_json::from_str(CODEX.lines().next().unwrap()).unwrap();
            header["payload"]["cwd"] = json!(self.cwd);
            let id = header["payload"]["id"].as_str().unwrap();
            let path = root
                .join("sessions/2026/11/19")
                .join(format!("rollout-2026-11-19T12-00-00-{id}.jsonl"));
            write(&path, &format!("{header}\n"));
            path
        }
    }

    fn write(path: &Path, data: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
    }

    fn append(path: &Path, data: &str) {
        fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(data.as_bytes())
            .unwrap();
    }

    fn until<T>(reader: &TranscriptReceiver, f: impl Fn() -> Option<T>) -> T {
        let until = Instant::now() + Duration::from_secs(8);
        loop {
            if let Some(value) = f() {
                return value;
            }
            assert!(
                !matches!(reader.status(), TurnStatus::Gap(_) | TurnStatus::Closed),
                "unexpected {:?}",
                reader.status()
            );
            assert!(Instant::now() < until, "timeout at {:?}", reader.status());
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn at_eof(reader: &TranscriptReceiver, path: &Path) {
        let length = path.metadata().unwrap().len();
        until(reader, || {
            matches!(reader.status(), TurnStatus::Active { offset, .. } if offset == length)
                .then_some(())
        });
    }

    fn next(reader: &TranscriptReceiver) -> TranscriptTurn {
        until(reader, || reader.try_recv())
    }

    #[test]
    fn same_cwd_and_session_id_in_two_isolated_codex_launches_do_not_cross() {
        let setup = Setup::new();
        let (root_a, lease_a) = setup.lease("first");
        let (root_b, lease_b) = setup.lease("second");
        let (_ta, a) = setup.attach(Source::Codex, &lease_a, TurnOptions::default());
        let (_tb, b) = setup.attach(Source::Codex, &lease_b, TurnOptions::default());
        let (spec, agent) = setup.spec(Source::Codex);
        assert!(matches!(
            AgentTerminal::spawn_with_transcript_turns(spec, agent, Source::Codex, &lease_a),
            Err(Error::InvalidInput(_))
        ));
        let pa = setup.codex_header(&root_a);
        let pb = setup.codex_header(&root_b);
        at_eof(&a, &pa);
        at_eof(&b, &pb);
        for (reader, path, text) in [(&b, &pb, "second reply"), (&a, &pa, "first reply")] {
            let lines = CODEX.lines().skip(1).collect::<Vec<_>>();
            append(path, &format!("{}\n", lines[0]));
            let decoys = [
                json!({"type":"response_item","payload":{"type":"reasoning","content":[{"type":"reasoning_text","text":"private"}]}}),
                json!({"type":"response_item","payload":{"type":"message","role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"intermediate"}]}}),
            ];
            for row in decoys {
                append(path, &format!("{row}\n"));
            }
            at_eof(reader, path);
            assert!(reader.try_recv().is_none());
            append(
                path,
                &format!(
                    "{}\n{}\n",
                    lines[1],
                    lines[2].replace("Sample confirmed.", text)
                ),
            );
            assert!(
                matches!(next(reader), TranscriptTurn::CodexFinal(reply) if reply.text == text)
            );
        }
        let add = CODEX.lines().skip(1).collect::<Vec<_>>().join("\n") + "\n";
        append(&pa, &add);
        at_eof(&a, &pa);
        lease_a.revoke();
        assert!(a.try_recv().is_none());
        assert_eq!(a.status(), TurnStatus::Gap(TurnGap::AuthorityRevoked));
        append(&pb, &add);
        at_eof(&b, &pb);
        let other = pb
            .parent()
            .unwrap()
            .join("rollout-x-22222222-2222-4222-8222-222222222222.jsonl");
        fs::copy(&pb, other).unwrap();
        assert!(b.try_recv().is_none());
        assert_eq!(b.status(), TurnStatus::Gap(TurnGap::Uncorroborated));
    }

    #[test]
    fn cursor_skips_copied_history_and_retains_distinct_repeated_replies_across_rewrites() {
        for source in [Source::CursorWithUnseparatedText, Source::Cursor] {
            let setup = Setup::new();
            let (root, lease) = setup.lease("cursor-data");
            let (_term, receiver) = setup.attach(source, &lease, TurnOptions::default());
            let path = setup.cursor_path(&root);
            write(&path, CURSOR[0]);
            at_eof(&receiver, &path);
            assert!(receiver.try_recv().is_none());
            for image in &CURSOR[1..] {
                write(&path, image);
                let event = next(&receiver);
                match (source, event) {
                    (
                        Source::CursorWithUnseparatedText,
                        TranscriptTurn::CursorUnseparatedText(reply),
                    ) => {
                        assert_eq!(reply.text, "Same visible reply.\n\nFixture reasoning.");
                        assert!(!format!("{reply:?}").contains("reasoning"));
                    }
                    (Source::Cursor, TranscriptTurn::CursorCompleted) => {}
                    (_, unexpected) => panic!("unexpected {unexpected:?}"),
                }
                at_eof(&receiver, &path);
                assert!(receiver.try_recv().is_none());
            }
            write(&path, &CURSOR[2].replace("Prompt 0", "Changed history"));
            until(&receiver, || {
                (receiver.status() == TurnStatus::Gap(TurnGap::Rewritten)).then_some(())
            });
            assert!(receiver.try_recv().is_none());
        }
    }

    #[test]
    fn pi_next_prompt_and_quiet_emit_only_stopped_assistant_text() {
        let setup = Setup::new();
        let (root, lease) = setup.lease("pi-sessions");
        let (_term, reader) = setup.attach(Source::Pi, &lease, TurnOptions::default());
        let path = root.join("2026-11-19T00-00-00-000Z_11111111-1111-4111-8111-111111111111.jsonl");
        let mut lines = PI.lines();
        write(
            &path,
            &(lines
                .next()
                .unwrap()
                .replace("/fixture/project", setup.cwd.to_str().unwrap())
                + "\n"),
        );
        at_eof(&reader, &path);
        append(&path, &(lines.collect::<Vec<_>>().join("\n") + "\n"));
        for expected in ["pi-proposal-reply-1", "pi-proposal-reply-2"] {
            assert!(
                matches!(next(&reader), TranscriptTurn::PiInferred(reply) if reply.text == expected)
            );
        }
        assert!(reader.try_recv().is_none());
    }

    #[test]
    fn launch_respects_explicit_provider_environment_and_rejects_imported_history() {
        let setup = Setup::new();
        let (root, lease) = setup.lease("leased");
        let other = setup.cwd.join("other");
        fs::create_dir(&other).unwrap();
        for variant in 0..3 {
            let (mut spec, agent) = setup.spec(Source::Codex);
            match variant {
                0 => {
                    spec.env.set("CODEX_HOME", &other);
                }
                1 => {
                    spec.env.remove("CODEX_HOME");
                }
                _ => {
                    spec.identity = TerminalIdentity::Custom {
                        vars: vec![("CODEX_HOME".into(), other.to_str().unwrap().into())],
                    };
                }
            }
            assert!(matches!(
                AgentTerminal::spawn_with_transcript_turns(spec, agent, Source::Codex, &lease),
                Err(Error::InvalidInput(_))
            ));
        }
        let (mut spec, agent) = setup.spec(Source::Codex);
        spec.env.set("CODEX_HOME", "leased");
        let (_term, _events) =
            AgentTerminal::spawn_with_transcript_turns(spec, agent, Source::Codex, &lease).unwrap();
        let (prepopulated, unused) = setup.lease("historical");
        setup.codex_header(&prepopulated);
        let (spec, agent) = setup.spec(Source::Codex);
        assert!(matches!(
            AgentTerminal::spawn_with_transcript_turns(spec, agent, Source::Codex, &unused),
            Err(Error::InvalidInput(_))
        ));
        assert!(root.is_dir());
    }
    #[test]
    fn cursor_withholds_a_queued_inference_during_a_short_rewrite_and_recovers() {
        let setup = Setup::new();
        let (root, lease) = setup.lease("cursor");
        let (_term, reader) = setup.attach(
            Source::CursorWithUnseparatedText,
            &lease,
            TurnOptions::default(),
        );
        let path = setup.cursor_path(&root);
        write(&path, CURSOR[0]);
        at_eof(&reader, &path);
        let without_end = CURSOR[1].rsplit_once("{\"type\":\"turn_ended\"").unwrap().0;
        write(&path, without_end);
        at_eof(&reader, &path);
        assert!(reader.try_recv().is_none());
        let next_user = CURSOR[2].lines().nth(4).unwrap();
        write(&path, &format!("{without_end}{next_user}\n"));
        at_eof(&reader, &path);
        write(&path, "");
        assert!(
            reader.try_recv().is_none(),
            "queued text escaped during truncation"
        );
        write(&path, CURSOR[2]);
        assert!(
            matches!(next(&reader),TranscriptTurn::CursorUnseparatedTextAtNextPrompt(r) if r.text=="Same visible reply.\n\nFixture reasoning.")
        );
        assert!(matches!(
            next(&reader),
            TranscriptTurn::CursorUnseparatedText(_)
        ));
        assert!(reader.try_recv().is_none());
    }

    #[test]
    fn cursor_image_limit_is_reported_before_exposing_any_history() {
        let setup = Setup::new();
        let (root, lease) = setup.lease("cursor");
        let (_term, reader) = setup.attach(
            Source::Cursor,
            &lease,
            TurnOptions {
                records: 2,
                ..TurnOptions::default()
            },
        );
        let path = setup.cursor_path(&root);
        write(&path, CURSOR[0]);
        until(&reader, || {
            (reader.status() == TurnStatus::Gap(TurnGap::Oversized)).then_some(())
        });
        assert!(reader.try_recv().is_none());
    }

    #[test]
    fn final_spawn_admission_rechecks_the_name_of_the_leased_root() {
        use std::cell::RefCell;
        use std::sync::Once;
        thread_local! { static REPLACE: RefCell<Option<PathBuf>> = const { RefCell::new(None) }; }
        struct ReplaceRoot;
        impl log::Log for ReplaceRoot {
            fn enabled(&self, _: &log::Metadata<'_>) -> bool {
                true
            }
            fn log(&self, record: &log::Record<'_>) {
                if record.target() == "portable_pty::cmdbuilder"
                    && record.args().to_string().contains("$SHELL ->")
                {
                    if let Some(path) = REPLACE.take() {
                        fs::rename(&path, path.with_file_name("original-root")).unwrap();
                        fs::create_dir(path).unwrap();
                    }
                }
            }
            fn flush(&self) {}
        }
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            log::set_logger(&ReplaceRoot).unwrap();
            log::set_max_level(log::LevelFilter::Debug);
        });
        let setup = Setup::new();
        let (root, lease) = setup.lease("codex-home");
        let (mut spec, agent) = setup.spec(Source::Codex);
        let shell = setup.cwd.join("unusable-shell");
        fs::write(&shell, []).unwrap();
        spec.env.set("SHELL", shell);
        REPLACE.set(Some(root));
        assert!(matches!(
            AgentTerminal::spawn_with_transcript_turns(spec, agent, Source::Codex, &lease),
            Err(Error::InvalidInput(_))
        ));
        assert!(REPLACE.take().is_none());
    }
    #[test]
    fn child_receives_a_provider_root_which_realpath_can_resolve() {
        use agent_link::Event;
        let setup = Setup::new();
        let (root, lease) = setup.lease("root");
        let bin = setup.cwd.join("env-printer");
        fs::create_dir(&bin).unwrap();
        symlink("/usr/bin/env", bin.join("codex")).unwrap();
        let mut spec = SpawnSpec::direct(
            bin.join("codex"),
            std::iter::empty::<&str>(),
            &setup.cwd,
            PtySize { rows: 24, cols: 80 },
        );
        spec.env.inherit = false;
        let (_term, events) = AgentTerminal::spawn_with_transcript_turns(
            spec,
            AgentSpec::new(Arc::new(CodexProfile)),
            Source::Codex,
            &lease,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut output = Vec::new();
        loop {
            match events.recv_deadline(deadline).unwrap() {
                Event::Output { bytes, .. } => output.extend(bytes),
                Event::Exit { .. } => break,
                _ => {}
            }
        }
        let text = String::from_utf8(output).unwrap();
        let exported = text
            .lines()
            .find_map(|line| line.strip_prefix("CODEX_HOME="))
            .unwrap();
        assert_eq!(fs::canonicalize(exported).unwrap(), root);
    }
}
