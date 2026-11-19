//! Recorded screen layouts and the keyboard transitions they can confirm.
use super::{Answer, AnswerError, KeyStep, PendingQuestion, QuestionItem};
use std::sync::Arc;
use std::time::Duration;

const PAUSE: Duration = Duration::from_millis(80);
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::agent) enum Provider {
    Claude,
    Codex,
    Cursor,
}

struct Page {
    item: QuestionItem,
    index: usize,
    total: usize,
    selected: Option<usize>,
    checked: Vec<bool>,
    // Text editor within the dedicated question. The empty draft is a distinct state.
    text: Option<String>,
}

fn numbered(row: &str) -> Option<(usize, bool, &str)> {
    let row = row.trim_start();
    let (current, row) = row
        .strip_prefix(['❯', '›'])
        .map_or((false, row), |r| (true, r.trim_start()));
    let (n, text) = row.split_once(". ")?;
    if n.is_empty() || n.len() > 2 || !n.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((
        n.parse::<usize>().ok()?.checked_sub(1)?,
        current,
        text.trim_end(),
    ))
}
fn claude_tabs(line: &str) -> Option<Vec<&str>> {
    if !line.contains('☐') && !line.contains('☒') {
        return None;
    }
    let tabs: Vec<_> = line
        .trim()
        .split("  ")
        .filter_map(|p| p.trim().strip_prefix(['☐', '☒']).map(str::trim))
        .collect();
    (!tabs.is_empty() && tabs.len() <= super::MAX_QUESTIONS).then_some(tabs)
}

fn page(provider: Provider, screen: &str) -> Option<Page> {
    if screen.len() > 128 * 1024 {
        return None;
    }
    let rows: Vec<_> = screen.lines().map(str::trim_end).collect();
    let (start, tabs, index, total) = match provider {
        Provider::Claude => {
            let n = rows.iter().rposition(|r| claude_tabs(r).is_some())?;
            let tabs = claude_tabs(rows[n])?;
            let total = tabs.len();
            (n, tabs, 0, total)
        }
        Provider::Codex => {
            let n = rows.iter().rposition(|r| {
                r.trim().starts_with("Question ") && r.trim().ends_with("unanswered)")
            })?;
            let positions = rows[n].trim().strip_prefix("Question ")?.split_once(' ')?.0;
            let (at, all) = positions.split_once('/')?;
            let index = at.parse::<usize>().ok()?.checked_sub(1)?;
            let total = all.parse::<usize>().ok()?;
            if index >= total || total > super::MAX_QUESTIONS {
                return None;
            }
            (n, Vec::new(), index, total)
        }
        Provider::Cursor => return None,
    };
    let end = rows[start + 1..].iter().position(|r| {
        let row = r.trim().to_ascii_lowercase();
        if provider == Provider::Claude {
            row.ends_with("esc to cancel")
        } else {
            row.ends_with("esc to interrupt")
                || row.starts_with("tab or esc to clear notes | enter to submit ")
        }
    })? + start
        + 1;
    if rows[end + 1..].iter().any(|r| !r.trim().is_empty()) {
        return None;
    }
    let editing = provider == Provider::Claude && rows[end].contains("ctrl+g to edit")
        || provider == Provider::Codex && rows[end].trim().starts_with("tab or esc to clear notes");
    let section = &rows[start + 1..end];
    let first = section.iter().position(|r| numbered(r).is_some())?;
    let prompt = section[..first]
        .iter()
        .map(|r| r.trim())
        .filter(|r| !r.is_empty())
        .collect::<Vec<_>>();
    if prompt.len() != 1 {
        return None;
    }
    let mut selected = None;
    let mut checked = Vec::new();
    let mut options = Vec::new();
    let mut text = None;
    let mut free_text = false;
    let mut multi = false;
    for row in &section[first..] {
        let Some((i, active, label)) = numbered(row) else {
            continue;
        };
        if active {
            if selected.is_some() {
                return None;
            }
            selected = Some(i);
        }
        if i != options.len() {
            break;
        }
        let (on, label) = if let Some(l) = label.strip_prefix("[✔] ") {
            multi = true;
            (true, l)
        } else if let Some(l) = label.strip_prefix("[ ] ") {
            multi = true;
            (false, l)
        } else {
            (false, label)
        };
        if provider == Provider::Claude {
            if editing && active {
                free_text = true;
                text = Some(if label == "Type something." {
                    String::new()
                } else {
                    label.to_owned()
                });
                break;
            }
            if label == "Type something." || label == "Type something" {
                free_text = true;
                break;
            }
            if label == "Chat about this" {
                break;
            }
            if let Some(draft) = label.strip_prefix("Type something: ") {
                free_text = true;
                text = Some(draft.to_owned());
                break;
            }
        } else if label.starts_with("None of the above  ") {
            free_text = true;
            break;
        }
        let label = if provider == Provider::Codex {
            label.split_once("  ")?.0.trim_end()
        } else {
            label
        };
        if label.is_empty() || options.len() >= super::MAX_OPTIONS {
            return None;
        }
        options.push(label.to_owned());
        checked.push(on);
    }
    if provider == Provider::Codex && editing {
        let draft = section
            .iter()
            .rev()
            .filter(|row| numbered(row).is_none())
            .find_map(|row| row.trim().strip_prefix("› "))?;
        text = Some(if draft == "Add notes" {
            String::new()
        } else {
            draft.to_owned()
        });
    }
    if !free_text || options.is_empty() {
        return None;
    }
    let header = tabs.first().copied().unwrap_or("");
    Some(Page {
        item: QuestionItem::new(header, prompt[0], options)
            .with_multi_select(multi)
            .with_free_text(free_text),
        index,
        total,
        selected,
        checked,
        text,
    })
}
fn same_item(page: &Page, item: &QuestionItem) -> bool {
    page.item.prompt == item.prompt
        && page.item.options == item.options
        && page.item.multi_select == item.multi_select
}
fn claude_review(screen: &str) -> Option<Vec<(&str, &str)>> {
    let (_, review) = screen.rsplit_once("\nReview your answers\n")?;
    let (details, tail) = review.split_once("Ready to submit your answers?")?;
    let menu = tail
        .lines()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .collect::<Vec<_>>();
    if menu != ["❯ 1. Submit answers", "2. Cancel"] {
        return None;
    }
    let lines = details
        .lines()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .collect::<Vec<_>>();
    if lines.is_empty() || lines.len() % 2 != 0 || lines.len() > 2 * super::MAX_QUESTIONS {
        return None;
    }
    lines
        .chunks_exact(2)
        .map(|p| Some((p[0].strip_prefix("● ")?, p[1].strip_prefix("→ ")?)))
        .collect()
}

pub(in crate::agent) fn shown(provider: Provider, screen: &str, q: &PendingQuestion) -> bool {
    if provider == Provider::Cursor {
        return cursor(screen).is_some_and(|v| v.questions == q.questions);
    }
    if provider == Provider::Claude {
        if let Some(review) = claude_review(screen) {
            return review.len() == q.questions.len()
                && review
                    .iter()
                    .zip(&q.questions)
                    .all(|((prompt, _), item)| *prompt == item.prompt);
        }
    }
    page(provider, screen).is_some_and(|p| {
        p.total == q.questions.len()
            && if provider == Provider::Codex {
                q.questions.get(p.index).is_some_and(|i| same_item(&p, i))
            } else {
                q.questions.iter().filter(|i| same_item(&p, i)).count() == 1
            }
    })
}

fn digit(n: usize) -> Result<Vec<u8>, AnswerError> {
    if n < 9 {
        Ok(vec![b'1' + n as u8])
    } else {
        Err(AnswerError::Unsupported)
    }
}
fn option_step(
    provider: Provider,
    item: &Arc<QuestionItem>,
    bytes: Vec<u8>,
    predicate: impl Fn(&Page) -> bool + Send + Sync + 'static,
) -> KeyStep {
    let item = Arc::clone(item);
    KeyStep::new(bytes, PAUSE).when(move |screen| {
        page(provider, screen).is_some_and(|p| same_item(&p, &item) && predicate(&p))
    })
}
fn text_bytes(text: &str) -> Result<Vec<u8>, AnswerError> {
    // A wrapped or multiline draft cannot be compared reliably in the recorded editor.
    if text.chars().any(char::is_control) {
        return Err(AnswerError::Unsupported);
    }
    Ok(format!("\x1b[200~{text}\x1b[201~").into_bytes())
}
fn review_values(q: &PendingQuestion, answers: &[Answer]) -> Vec<(String, String)> {
    q.questions
        .iter()
        .zip(answers)
        .map(|(item, a)| {
            let value = match a {
                Answer::Choice(i) => item.options[*i].clone(),
                Answer::Text(s) => s.clone(),
                Answer::Choices(v) => item
                    .options
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| v.contains(i))
                    .map(|(_, s)| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            };
            (item.prompt.clone(), value)
        })
        .collect()
}

pub(in crate::agent) fn keys(
    provider: Provider,
    screen: &str,
    q: &PendingQuestion,
    answers: &[Answer],
) -> Result<Vec<KeyStep>, AnswerError> {
    if !q.valid() {
        return Err(AnswerError::Unsupported);
    }
    super::dispatch::validate(q, answers)?;
    if provider == Provider::Cursor {
        if !shown(provider, screen, q) {
            return Err(AnswerError::Changed);
        }
        let [Answer::Choice(i)] = answers else {
            return Err(AnswerError::Unsupported);
        };
        // Global permission-mode changes require a distinct verified interaction.
        let bytes = match q.questions[0].options[*i].to_ascii_lowercase().as_str() {
            "run (once) (y)" => b"y".as_slice(),
            "skip & tell the agent what to do instead (esc or n)" => b"n".as_slice(),
            _ => return Err(AnswerError::Unsupported),
        };
        return Ok(vec![KeyStep::new(bytes, Duration::ZERO)]);
    }
    let first = page(provider, screen).ok_or(AnswerError::Changed)?;
    if !same_item(&first, &q.questions[0]) || first.total != answers.len() || first.index != 0 {
        return Err(AnswerError::Changed);
    }
    let mut steps = Vec::new();
    let mut push = |step| {
        if steps.len() == super::MAX_STEPS {
            return Err(AnswerError::InputTooLarge);
        }
        steps.push(step);
        Ok(())
    };
    for (at, (item, answer)) in q.questions.iter().zip(answers).enumerate() {
        let item = &Arc::new(item.clone());
        if provider == Provider::Claude {
            if item.multi_select {
                let mut checked = if at == 0 {
                    first.checked.clone()
                } else {
                    vec![false; item.options.len()]
                };
                let selected = match answer {
                    Answer::Choice(i) => vec![*i],
                    Answer::Choices(v) => v.clone(),
                    Answer::Text(_) => return Err(AnswerError::Unsupported),
                };
                for index in 0..checked.len() {
                    if checked[index] == selected.contains(&index) {
                        continue;
                    }
                    let expected = checked.clone();
                    push(option_step(provider, item, digit(index)?, move |p| {
                        p.checked == expected
                    }))?;
                    checked[index] = !checked[index];
                }
                push(option_step(provider, item, vec![b'\t'], move |p| {
                    p.checked == checked
                }))?;
            } else {
                match answer {
                    Answer::Choice(i) => push(option_step(provider, item, digit(*i)?, |p| {
                        p.text.is_none()
                    }))?,
                    Answer::Text(text) => {
                        push(option_step(
                            provider,
                            item,
                            digit(item.options.len())?,
                            |p| p.text.is_none(),
                        ))?;
                        push(option_step(provider, item, text_bytes(text)?, |p| {
                            p.text.as_deref() == Some("")
                        }))?;
                        let text = text.clone();
                        push(option_step(provider, item, vec![b'\r'], move |p| {
                            p.text.as_ref() == Some(&text)
                        }))?;
                    }
                    _ => return Err(AnswerError::Unsupported),
                }
            }
        } else {
            let from = if at == 0 {
                first.selected.ok_or(AnswerError::Changed)?
            } else {
                0
            };
            let to = match answer {
                Answer::Choice(i) => *i,
                Answer::Text(_) => item.options.len(),
                _ => return Err(AnswerError::Unsupported),
            };
            for n in 0..from.abs_diff(to) {
                let (pos, bytes) = if to > from {
                    (from + n, b"\x1b[B")
                } else {
                    (from - n, b"\x1b[A")
                };
                push(option_step(provider, item, bytes.to_vec(), move |p| {
                    p.index == at && p.selected == Some(pos) && p.text.is_none()
                }))?;
            }
            if let Answer::Text(text) = answer {
                push(option_step(provider, item, vec![b'\t'], move |p| {
                    p.index == at && p.selected == Some(to) && p.text.is_none()
                }))?;
                push(option_step(provider, item, text_bytes(text)?, move |p| {
                    p.index == at && p.selected == Some(to) && p.text.as_deref() == Some("")
                }))?;
                let value = text.clone();
                push(option_step(provider, item, vec![b'\r'], move |p| {
                    p.index == at && p.selected == Some(to) && p.text.as_ref() == Some(&value)
                }))?;
            } else {
                push(option_step(provider, item, vec![b'\r'], move |p| {
                    p.index == at && p.selected == Some(to) && p.text.is_none()
                }))?;
            }
        }
    }
    if provider == Provider::Claude
        && (q.questions.len() > 1 || q.questions.iter().any(|i| i.multi_select))
    {
        let expected = review_values(q, answers);
        push(
            KeyStep::new(vec![b'\r'], Duration::ZERO).when(move |screen| {
                claude_review(screen).is_some_and(|actual| {
                    actual.len() == expected.len()
                        && actual
                            .iter()
                            .zip(&expected)
                            .all(|((q, a), (p, v))| *q == p && *a == v)
                })
            }),
        )?;
    }
    Ok(steps)
}

pub(in crate::agent) fn cursor(screen: &str) -> Option<PendingQuestion> {
    if screen.len() > 128 * 1024 {
        return None;
    }
    let (above, menu) = screen.rsplit_once("\n Run this command?\n")?;
    let mut rows = above.lines().rev().map(str::trim).filter(|s| !s.is_empty());
    let prompt = rows.next()?.strip_prefix("$  ")?;
    let (command, cwd) = prompt.rsplit_once(" in ")?;
    if command.is_empty()
        || cwd.is_empty()
        || prompt.contains('…')
        || prompt.contains("...")
        || !rows.next()?.chars().all(|c| c == '─')
    {
        return None;
    }
    let options = menu
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.strip_prefix("→ ").unwrap_or(s))
        .collect::<Vec<_>>();
    if options
        != [
            "Run (once) (y)",
            "Skip & tell the agent what to do instead (esc or n)",
        ]
    {
        return None;
    }
    let q = PendingQuestion::new(
        "",
        vec![QuestionItem::new(
            "Run this command?",
            prompt,
            options.iter().map(|s| s.to_string()).collect(),
        )],
    );
    q.valid().then_some(q)
}
