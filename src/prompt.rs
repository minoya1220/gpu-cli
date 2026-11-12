//! Line-based prompts used when stdin or stdout is not a terminal.

use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::history::{by_frequent_recency, recent_picks};
use crate::offers::{category_label, group_by_category, model_label, Stats};
use crate::picker::AnyJob;
use crate::pyfmt::{casefold, is_digits, repr_str};

const PAGE_SIZE: usize = 12;

/// Options plus how to label them and match them against history.
pub struct Choices<'a, T> {
    pub options: Vec<T>,
    pub labels: Vec<String>,
    /// An option's text as stored in history; `None` never matches.
    pub key: &'a dyn Fn(&T) -> Option<String>,
}

impl<'a> Choices<'a, String> {
    pub fn strings(options: Vec<String>) -> Choices<'a, String> {
        let labels = options.clone();
        Choices {
            options,
            labels,
            key: &|s: &String| Some(s.clone()),
        }
    }

    pub fn labelled(options: Vec<String>, labels: Vec<String>) -> Choices<'a, String> {
        Choices {
            options,
            labels,
            key: &|s: &String| Some(s.clone()),
        }
    }
}

fn number(answer: &str) -> Option<usize> {
    if is_digits(answer) {
        answer.parse::<usize>().ok().or(Some(usize::MAX))
    } else {
        None
    }
}

/// Line-based picker: numbered pages of 12, typed filters, `n`/`p` paging,
/// and Enter for the most recent pick (or the first match).
pub fn choose<T: Clone + PartialEq>(
    ctx: &Ctx,
    choices: &Choices<T>,
    prompt: &str,
    step: Option<&str>,
    filter_across: Option<&Choices<T>>,
) -> Result<T> {
    let (options, labels) = (&choices.options, &choices.labels);
    if options.is_empty() {
        return Err(Error::msg(format!(
            "No choices available for {}",
            prompt.to_lowercase()
        )));
    }
    let history = step.map(|s| recent_picks(&ctx.env, s)).unwrap_or_default();
    let recent: Vec<T> = by_frequent_recency(options, &history, choices.key)
        .into_iter()
        .take(5)
        .collect();
    let index_of = |option: &T| options.iter().position(|o| o == option).unwrap_or(0);
    let mut page = 0usize;
    let mut filter = String::new();
    loop {
        let needle = casefold(&filter);
        let filtered: Vec<usize> = labels
            .iter()
            .enumerate()
            .filter(|(_, l)| casefold(l).contains(&needle))
            .map(|(i, _)| i)
            .collect();
        if !filter.is_empty() && filtered.len() == 1 {
            ctx.println(&format!("Unique match: {}", labels[filtered[0]]))?;
            return Ok(options[filtered[0]].clone());
        }
        let pages = filtered.len().div_ceil(PAGE_SIZE).max(1);
        page = page.min(pages - 1);
        if !recent.is_empty() && filter.is_empty() {
            ctx.println("Recent/frequent:")?;
            for (i, option) in recent.iter().enumerate() {
                ctx.println(&format!("  {}. {}", i + 1, labels[index_of(option)]))?;
            }
        }
        let shown_filter = if filter.is_empty() {
            String::new()
        } else {
            format!(" filter={}", repr_str(&filter))
        };
        ctx.println(&format!(
            "{prompt} (page {}/{pages}){shown_filter}",
            page + 1
        ))?;
        let start = page * PAGE_SIZE;
        for (n, index) in filtered.iter().skip(start).take(PAGE_SIZE).enumerate() {
            ctx.println(&format!("{}. {}", n + 1, labels[*index]))?;
        }
        let latest = history.iter().find_map(|item| {
            options
                .iter()
                .position(|o| (choices.key)(o).as_deref() == Some(item.as_str()))
        });
        let default = latest.or_else(|| filtered.first().copied());
        let usable_default = default.filter(|d| filter.is_empty() || filtered.contains(d));
        let hint = match usable_default {
            Some(index) => format!(", Enter = {}", labels[index]),
            None => String::new(),
        };
        let prompt_text = format!("Select number, text to filter, n/p page{hint}: ");
        let answer = ctx.console.input(&prompt_text)?.ok_or_else(|| {
            let flag = match step {
                Some("gpu") => "--gpu",
                Some("mode") => "--mode",
                _ if prompt.to_lowercase().contains("repository") => "--repo or --no-repo",
                _ if prompt.to_lowercase().contains("offer") => "--offer",
                _ if prompt.to_lowercase().contains("mode") => "--mode",
                _ => "the corresponding flag",
            };
            Error::msg(format!(
                "Input ended at {prompt}. Pass {flag} to choose without a prompt."
            ))
        })?;
        let answer = answer.trim();
        let on_page = PAGE_SIZE.min(filtered.len().saturating_sub(start));
        if answer.is_empty() {
            if let Some(default) = usable_default {
                return Ok(options[default].clone());
            }
            ctx.println("No default in this filter; choose a listed number.")?;
        } else if casefold(answer) == "n" && page + 1 < pages {
            page += 1;
        } else if casefold(answer) == "p" && page > 0 {
            page -= 1;
        } else if number(answer).is_some_and(|n| 1 <= n && n <= on_page) {
            let n = number(answer).unwrap_or(1);
            return Ok(options[filtered[start + n - 1]].clone());
        } else if !is_digits(answer) {
            match filter_across {
                Some(all) => {
                    let needle = casefold(answer);
                    let matches: Vec<usize> = all
                        .labels
                        .iter()
                        .enumerate()
                        .filter(|(_, l)| casefold(l).contains(&needle))
                        .map(|(i, _)| i)
                        .collect();
                    if matches.len() == 1 {
                        return Ok(all.options[matches[0]].clone());
                    }
                    if !matches.is_empty() {
                        let narrowed = Choices {
                            options: matches.iter().map(|i| all.options[*i].clone()).collect(),
                            labels: matches.iter().map(|i| all.labels[*i].clone()).collect(),
                            key: all.key,
                        };
                        return choose(ctx, &narrowed, "GPU model filter", Some("gpu"), None);
                    }
                    ctx.println("No GPU models match that filter.")?;
                }
                None => {
                    filter = answer.to_string();
                    page = 0;
                }
            }
        } else {
            ctx.println("Choose a listed number, filter text, n, or p.")?;
        }
    }
}

/// Line-based GPU prompt: recent models, categories, or a typed model filter.
pub fn choose_gpu(ctx: &Ctx, models: &[String], stats: &Stats) -> Result<String> {
    let history = recent_picks(&ctx.env, "gpu");
    let recent: Vec<String> = by_frequent_recency(models, &history, |m: &String| Some(m.clone()))
        .into_iter()
        .take(5)
        .collect();
    let categories = group_by_category(models);
    let all = Choices::labelled(
        models.to_vec(),
        models.iter().map(|m| model_label(m, stats)).collect(),
    );
    loop {
        if !recent.is_empty() {
            ctx.println("Recent/frequent GPU models:")?;
            for (n, model) in recent.iter().enumerate() {
                ctx.println(&format!("{}. {}", n + 1, model_label(model, stats)))?;
            }
        }
        for (n, (name, members)) in categories.iter().enumerate() {
            ctx.println(&format!(
                "{}. {}",
                n + recent.len() + 1,
                category_label(name, members, stats)
            ))?;
        }
        ctx.println("Type a model substring to filter all GPUs, or choose a category.")?;
        let has_default = history.first().is_some_and(|h| models.contains(h));
        let prompt = if has_default {
            format!("GPU [Enter = {}]: ", history[0])
        } else {
            "GPU: ".into()
        };
        let answer = ctx.console.input(&prompt)?.ok_or_else(|| {
            Error::msg(
                "Input ended at GPU selection. Pass --gpu 'RTX 4090' to choose without a prompt.",
            )
        })?;
        let answer = answer.trim();
        if answer.is_empty() && has_default {
            return Ok(history[0].clone());
        }
        if let Some(n) = number(answer) {
            if 1 <= n && n <= recent.len() {
                return Ok(recent[n - 1].clone());
            }
            if recent.len() < n && n <= recent.len() + categories.len() {
                let (name, members) = &categories[n - recent.len() - 1];
                let choices = Choices::labelled(
                    members.clone(),
                    members.iter().map(|m| model_label(m, stats)).collect(),
                );
                return choose(ctx, &choices, name, Some("gpu"), Some(&all));
            }
        }
        let needle = casefold(answer);
        let matches: Vec<String> = if answer.is_empty() {
            Vec::new()
        } else {
            models
                .iter()
                .filter(|m| casefold(m).contains(&needle))
                .cloned()
                .collect()
        };
        if matches.len() == 1 {
            return Ok(matches[0].clone());
        }
        if !matches.is_empty() {
            let labels = matches.iter().map(|m| model_label(m, stats)).collect();
            return choose(
                ctx,
                &Choices::labelled(matches, labels),
                "GPU model filter",
                Some("gpu"),
                None,
            );
        }
        ctx.println("Choose a listed category or type a matching model substring.")?;
    }
}

/// Plain line input off-TTY, raw picker input on-TTY. Decoding arrows here
/// prevents canonical-mode echo in standalone setup/destruction prompts too.
pub(crate) fn read_text(ctx: &Ctx, prompt: &str) -> Result<String> {
    if !ctx.interactive() {
        return ctx.input_or(prompt, Error::Cancelled(1));
    }
    crate::picker::pick(
        ctx,
        crate::picker::Screen::new(prompt.trim_end(), Vec::new, crate::picker::Next::Done)
            .text_input(None)
            .input_label("Reply")
            .submit_on_right(false)
            .back_clears_input(false),
    )
}

#[derive(Clone)]
pub(crate) struct Confirmation<T> {
    pub title: String,
    pub value: T,
}

/// A launch approval is a normal page on its parent's stack. No slow work
/// runs here: the Job supplies an immutable plan and the exact quote shown.
pub(crate) fn confirmation_screen<V: crate::picker::PickValue, T: Clone + Send + Sync + 'static>(
    job: crate::picker::Job<Confirmation<T>>,
) -> crate::picker::Screen<V, T> {
    let title = job.clone();
    let selected = job.clone();
    crate::picker::Screen::new("Confirm launch", Vec::new, move |_| {
        match selected.result() {
            Some(confirmation) => crate::picker::Next::Done(confirmation.value),
            None => crate::picker::Next::Notice("Wait for launch checks".into()),
        }
    })
    .title_with(move || {
        title
            .result()
            .map(|c| crate::redact::redact_text(&c.title))
            .unwrap_or_else(|| {
                if title.state() == crate::picker::JobState::Failed {
                    "Launch unavailable"
                } else {
                    "Checking launch..."
                }
                .into()
            })
    })
    .load(job)
    .loading("Checking launch...")
    .text_input(Some(Box::new(|text| {
        if text.trim().eq_ignore_ascii_case("yes") {
            Ok(())
        } else {
            Err(CONFIRM.into())
        }
    })))
    .input_label(CONFIRM)
    .submit_on_right(false)
    .back_clears_input(false)
    .settle_active_only()
}

/// The one instruction every paid or destructive confirmation shows.
pub(crate) const CONFIRM: &str = "Type 'yes' to confirm";

/// Ask `question` (or only the instruction, after a question already
/// printed); true when the answer is "yes". End of input cancels with status 1.
pub fn confirm(ctx: &Ctx, question: Option<&str>) -> Result<bool> {
    let prompt = match question {
        Some(question) => format!("{question} {CONFIRM}: "),
        None => format!("{CONFIRM}: "),
    };
    let answer = read_text(ctx, &prompt)?;
    Ok(answer.trim().to_lowercase() == "yes")
}
