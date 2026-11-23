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
        TurnGap, TurnOptions, TurnRecvError, TurnStatus,
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

    /// Deadline for one blocking or awaited wait below.
    const LIMIT: Duration = Duration::from_secs(8);

    fn codex_turn(path: &Path, text: &str) {
        let lines = CODEX.lines().skip(1).collect::<Vec<_>>();
        let end = lines[2].replace("Sample confirmed.", text);
        append(path, &format!("{}\n{}\n{end}\n", lines[0], lines[1]));
    }

    #[test]
    fn blocking_waits_follow_binding_replies_and_revocation() {
        let setup = Setup::new();
        let (root, lease) = setup.lease("waits");
        let (_term, reader) = setup.attach(Source::Codex, &lease, TurnOptions::default());
        assert_eq!(
            reader.recv_timeout(Duration::ZERO).unwrap_err(),
            TurnRecvError::Timeout
        );
        std::thread::scope(|s| {
            let bound = s.spawn(|| reader.wait_status_change(TurnStatus::Waiting, LIMIT));
            let path = setup.codex_header(&root);
            let status = bound.join().unwrap();
            assert!(
                matches!(status, Some(TurnStatus::Active { .. })),
                "{status:?}"
            );
            let next = s.spawn(|| reader.recv_timeout(LIMIT));
            codex_turn(&path, "waited");
            assert!(matches!(
                next.join().unwrap(),
                Ok(TranscriptTurn::CodexFinal(reply)) if reply.text == "waited"
            ));
            let revoked = s.spawn(|| reader.recv_timeout(LIMIT));
            lease.revoke();
            assert_eq!(
                revoked.join().unwrap().unwrap_err(),
                TurnRecvError::Gap(TurnGap::AuthorityRevoked)
            );
        });
        let gap = TurnStatus::Gap(TurnGap::AuthorityRevoked);
        assert_eq!(reader.wait_status_change(gap, LIMIT), Some(gap));
    }

    #[cfg(feature = "tokio")]
    #[tokio::test]
    async fn awaited_waits_follow_binding_replies_and_revocation() {
        use tokio::time::timeout;

        let setup = Setup::new();
        let (root, lease) = setup.lease("awaits");
        let (_term, reader) = setup.attach(Source::Codex, &lease, TurnOptions::default());
        let path = setup.codex_header(&root);
        let status = timeout(LIMIT, reader.status_change(TurnStatus::Waiting))
            .await
            .unwrap();
        assert!(matches!(status, TurnStatus::Active { .. }), "{status:?}");
        codex_turn(&path, "awaited");
        let turn = timeout(LIMIT, reader.recv()).await.unwrap();
        assert!(matches!(turn, Ok(TranscriptTurn::CodexFinal(reply)) if reply.text == "awaited"));
        let (revoked, ()) = tokio::join!(timeout(LIMIT, reader.recv()), async {
            lease.revoke();
        });
        assert_eq!(
            revoked.unwrap().unwrap_err(),
            TurnRecvError::Gap(TurnGap::AuthorityRevoked)
        );
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

    #[cfg(feature = "questions")]
    #[test]
    fn codex_questions_and_final_replies_share_one_verified_transcript() {
        use agent_link::Event;
        use agent_link::agent::questions::{Answer, ClosedBy};

        let setup = Setup::new();
        let (root, lease) = setup.lease("question-root");
        let bin = setup.cwd.join("mock-cli");
        fs::create_dir(&bin).unwrap();
        symlink("/bin/zsh", bin.join("codex")).unwrap();
        // The foreground process replays the recorded question screen and acknowledges
        // the first answer byte. Transcript writes below are explicitly controlled.
        let screen = include_str!("fixtures/codex-0.154.0-question-plan.txt");
        write(
            &setup.cwd.join("screen"),
            &screen.trim_end().replace('\n', "\r\n"),
        );
        write(
            &setup.cwd.join(".zshenv"),
            r#"unsetopt monitor
stty raw -echo || exit 2
printf '\033]0;question-replay-ready\007'
read -rk 1 gate || exit 3
cat screen || exit 4
dd bs=1 count=1 of=received 2>/dev/null || exit 5
printf '\033[2J\033[H\033]0;answer-received\007'
read -rk 1 end
exit 0
"#,
        );
        let mut spec = SpawnSpec::direct(
            bin.join("codex"),
            std::iter::empty::<&str>(),
            &setup.cwd,
            PtySize {
                rows: 40,
                cols: 120,
            },
        );
        spec.env.inherit = false;
        spec.env.set("HOME", &setup.cwd);
        spec.env.set("ZDOTDIR", &setup.cwd);
        spec.env.set("PATH", "/usr/bin:/bin");
        let agent = AgentSpec::new(Arc::new(CodexProfile));
        let (term, events) =
            AgentTerminal::spawn_with_transcript_turns(spec, agent, Source::Codex, &lease).unwrap();
        let reader = term
            .attach_transcript_turns(TurnOptions::default())
            .unwrap();
        let path = setup.codex_header(&root);
        at_eof(&reader, &path);
        {
            let (_, pi_root) = setup.lease("unrelated-question-source");
            let (other_terminal, pi) = setup.attach(Source::Pi, &pi_root, TurnOptions::default());
            assert!(matches!(
                other_terminal.observe_provider_questions(&reader),
                Err(Error::InvalidInput(_))
            ));
            assert!(matches!(
                term.observe_provider_questions(&pi),
                Err(Error::Unsupported(_))
            ));
        }
        term.observe_provider_questions(&reader).unwrap();
        until(&reader, || {
            term.terminal()
                .screen()
                .filter(|s| s.title == "question-replay-ready")
        });
        let mut calls = include_str!("fixtures/questions/codex-0.154.0.jsonl").lines();
        let mut turn = CODEX.lines().skip(1);
        append(
            &path,
            &format!("{}\n{}\n", turn.next().unwrap(), calls.next().unwrap()),
        );
        term.terminal().write(b"!").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let remaining = || {
            deadline
                .checked_duration_since(Instant::now())
                .expect("question deadline")
        };
        let id = loop {
            if let Event::Question { qid, questions, .. } =
                events.recv_timeout(remaining()).unwrap()
            {
                assert_eq!(questions[0].options, ["Blue", "Amber"]);
                break qid;
            }
        };
        assert_eq!(
            term.answer_question(id, vec![Answer::Choice(0)])
                .wait(remaining()),
            Some(Ok(()))
        );
        until(&reader, || {
            term.terminal()
                .screen()
                .filter(|s| s.title == "answer-received")
        });
        assert_eq!(fs::read(setup.cwd.join("received")).unwrap(), b"\r");
        append(
            &path,
            &format!(
                "{}\n{}\n{}\n",
                calls.next().unwrap(),
                turn.next().unwrap(),
                turn.next().unwrap()
            ),
        );
        loop {
            if let Event::QuestionClosed { qid, by, .. } = events.recv_timeout(remaining()).unwrap()
            {
                assert_eq!((qid, by), (id, ClosedBy::Host));
                break;
            }
        }
        assert!(
            matches!(next(&reader), TranscriptTurn::CodexFinal(reply) if reply.text == "Sample confirmed.")
        );
    }
}
