use std::path::{Path, PathBuf};

use crate::agent::turns::{Claim, Gap, ProviderId, TranscriptSource};

const ENTRIES: usize = 256;

pub(super) fn uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
            }
        })
}

fn cursor_project(cwd: &Path) -> Result<PathBuf, Gap> {
    let cwd = cwd
        .to_str()
        .filter(|s| s.is_ascii() && s.len() <= 200)
        .ok_or(Gap::Uncorroborated)?;
    let mut slug = String::new();
    for part in cwd
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|p| !p.is_empty())
    {
        if !slug.is_empty() {
            slug.push('-');
        }
        slug.push_str(part);
    }
    if slug.is_empty() {
        return Err(Gap::Uncorroborated);
    }
    Ok(Path::new("projects").join(slug).join("agent-transcripts"))
}

pub(super) struct SessionFile {
    pub(super) path: PathBuf,
    pub(super) id: ProviderId,
}

struct Search<'a> {
    claim: &'a Claim,
    remaining: usize,
    found: Option<SessionFile>,
}

impl Search<'_> {
    fn names(&mut self, dir: &Path) -> Result<Vec<std::ffi::OsString>, Gap> {
        match self.claim.root.names(dir, &mut self.remaining) {
            Err(Gap::Missing) => Ok(Vec::new()),
            other => other,
        }
    }

    fn add(&mut self, path: PathBuf, id: &str) -> Result<(), Gap> {
        if !uuid(id) || self.found.is_some() {
            return Err(Gap::Uncorroborated);
        }
        self.found = Some(SessionFile {
            path,
            id: ProviderId::new(id)?,
        });
        Ok(())
    }

    fn codex(&mut self, dir: &Path, depth: usize) -> Result<(), Gap> {
        for word in self.names(dir)? {
            let name = word.to_str().ok_or(Gap::Uncorroborated)?;
            let path = dir.join(name);
            if depth < 3 {
                if name.len() == if depth == 0 { 4 } else { 2 }
                    && name.bytes().all(|b| b.is_ascii_digit())
                {
                    self.codex(&path, depth + 1)?;
                }
            } else if let Some(stem) = name
                .strip_prefix("rollout-")
                .and_then(|s| s.strip_suffix(".jsonl"))
            {
                let id = stem
                    .get(stem.len().saturating_sub(36)..)
                    .ok_or(Gap::Uncorroborated)?;
                self.add(path, id)?;
            }
        }
        Ok(())
    }

    fn pi(&mut self) -> Result<(), Gap> {
        for word in self.names(Path::new(""))? {
            let name = word.to_str().ok_or(Gap::Uncorroborated)?;
            if let Some(stem) = name.strip_suffix(".jsonl") {
                let (_, id) = stem.rsplit_once('_').ok_or(Gap::Uncorroborated)?;
                self.add(PathBuf::from(name), id)?;
            }
        }
        Ok(())
    }

    fn cursor(&mut self) -> Result<(), Gap> {
        let base = cursor_project(&self.claim.cwd)?;
        for word in self.names(&base)? {
            let id = word
                .to_str()
                .filter(|s| uuid(s))
                .ok_or(Gap::Uncorroborated)?;
            let path = base.join(id).join(format!("{id}.jsonl"));
            // A second directory alone is an ambiguous writer/session, even if its
            // file is still being created.
            self.add(path, id)?;
        }
        Ok(())
    }
}

pub(super) fn locate(claim: &Claim) -> Result<Option<SessionFile>, Gap> {
    let mut search = Search {
        claim,
        remaining: ENTRIES,
        found: None,
    };
    match claim.source.ok_or(Gap::Uncorroborated)? {
        TranscriptSource::Codex => search.codex(Path::new("sessions"), 0)?,
        TranscriptSource::Pi => search.pi()?,
        TranscriptSource::Cursor | TranscriptSource::CursorWithUnseparatedText => {
            search.cursor()?
        }
    }
    Ok(search.found)
}
