//! Shared two-level zone picker. Callers supply geographically ordered zones;
//! area order follows the nearest member, without a second geography table.
use crate::{
    gcp::catalog::Zone,
    picker::{Job, Next, Opt, Screen},
};
use std::rc::Rc;

pub fn area(zone: &str) -> String {
    let prefix = crate::gcp::geo::region(zone)
        .unwrap_or(zone)
        .trim_end_matches(|c: char| c.is_ascii_digit());
    // The requested South America umbrella covers every numbered/directional
    // region; other areas retain their geographic prefix, with no region list.
    if prefix.starts_with("southamerica-") {
        "southamerica".into()
    } else {
        prefix.into()
    }
}
fn groups(zones: &[Zone]) -> Vec<(String, usize)> {
    let mut result: Vec<(String, usize)> = Vec::new();
    for zone in zones {
        let name = area(&zone.name);
        if let Some((_, count)) = result.iter_mut().find(|(a, _)| *a == name) {
            *count += 1;
        } else {
            result.push((name, 1));
        }
    }
    result
}
type PriceFields = Rc<dyn Fn(&Zone) -> [String; 2]>;
#[derive(Clone)]
pub(crate) struct Presentation {
    pub preferred: Option<String>,
    pub fields: PriceFields,
    pub prices: Option<Job<()>>,
}
impl Default for Presentation {
    fn default() -> Self {
        Self {
            preferred: None,
            fields: Rc::new(|_| [String::new(), String::new()]),
            prices: None,
        }
    }
}
fn options(zones: Vec<Zone>, display: &Presentation) -> Vec<Opt<String>> {
    let rows = zones
        .iter()
        .map(|z| {
            let [price, cache] = (display.fields)(z);
            [z.name.clone(), price, cache]
        })
        .collect::<Vec<_>>();
    let labels = crate::table::aligned_rows(
        &rows,
        [
            crate::table::Column::Left,
            crate::table::Column::Right,
            crate::table::Column::Plain,
        ],
    );
    zones
        .into_iter()
        .zip(labels)
        .map(|(z, label)| Opt::new(z.name, label.trim_end().to_string()))
        .collect()
}
type Select<R> = Rc<dyn Fn(String) -> Next<String, R>>;
#[cfg(test)]
pub fn screen<R: 'static>(
    job: Job<Vec<Zone>>,
    initial: Vec<Zone>,
    title: &str,
    notice: &str,
    select: impl Fn(String) -> Next<String, R> + 'static,
) -> Screen<String, R> {
    present(job, initial, title, notice, Presentation::default(), select)
}
pub(crate) fn present<R: 'static>(
    job: Job<Vec<Zone>>,
    initial: Vec<Zone>,
    title: &str,
    notice: &str,
    display: Presentation,
    select: impl Fn(String) -> Next<String, R> + 'static,
) -> Screen<String, R> {
    build(
        job,
        initial,
        title.into(),
        notice.into(),
        display,
        Rc::new(select),
        None,
    )
}

fn build<R: 'static>(
    job: Job<Vec<Zone>>,
    initial: Vec<Zone>,
    title: String,
    notice: String,
    display: Presentation,
    select: Select<R>,
    group: Option<String>,
) -> Screen<String, R> {
    let load = job.clone();
    let fallback = initial.clone();
    let selected = group.clone();
    let data = move || {
        let mut zones = load.result().unwrap_or_else(|| fallback.clone());
        if let Some(a) = &selected {
            zones.retain(|z| area(&z.name) == *a);
        }
        zones
    };
    let labels = data.clone();
    let highlight = data.clone();
    let highlighting = display.clone();
    let highlight_group = group.clone();
    let search = data;
    let search_display = display.clone();
    let label_display = display.clone();
    let child_display = display.clone();
    let child_notice = notice.clone();
    let picked = select.clone();
    let nested = job.clone();
    let nested_initial = initial;
    let mut screen = Screen::new(
        title,
        move || {
            let zones = labels();
            if group.is_some() {
                options(zones, &label_display)
            } else {
                let areas = groups(&zones);
                let rows = areas
                    .iter()
                    .map(|(name, count)| {
                        [
                            name.clone(),
                            count.to_string(),
                            crate::table::noun(*count, "zone"),
                        ]
                    })
                    .collect::<Vec<_>>();
                let labels = crate::table::aligned_rows(
                    &rows,
                    [
                        crate::table::Column::Left,
                        crate::table::Column::Right,
                        crate::table::Column::Plain,
                    ],
                );
                areas
                    .into_iter()
                    .zip(labels)
                    .map(|((name, _), label)| Opt::new(format!("@area:{name}"), label))
                    .collect()
            }
        },
        move |value: String| {
            if let Some(a) = value.strip_prefix("@area:") {
                Next::Screen(build(
                    nested.clone(),
                    nested_initial.clone(),
                    format!("{a} zones"),
                    child_notice.clone(),
                    child_display.clone(),
                    picked.clone(),
                    Some(a.into()),
                ))
            } else {
                picked(value)
            }
        },
    )
    .notice(notice)
    .load(job)
    .load_required(false)
    .quick_select(false)
    .loading("Checking TPU zones…")
    .empty("No TPU zones returned. Ctrl-R retries.");
    screen = screen
        .search(move || options(search(), &search_display))
        .highlight(move || {
            let zones = highlight();
            let Some(preferred) = highlighting
                .preferred
                .as_ref()
                .filter(|p| zones.iter().any(|z| z.name == **p))
            else {
                return 0;
            };
            if highlight_group.is_some() {
                zones.iter().position(|z| z.name == *preferred).unwrap_or(0)
            } else {
                let wanted = area(preferred);
                groups(&zones)
                    .iter()
                    .position(|(g, _)| *g == wanted)
                    .unwrap_or(0)
            }
        });
    if let Some(job) = display.prices {
        screen = screen.prices(job);
    }
    screen
}

#[cfg(test)]
mod tests;
