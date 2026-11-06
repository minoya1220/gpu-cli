//! Shared two-level zone picker. Callers supply geographically ordered zones;
//! area order follows the nearest member, without a second geography table.
use crate::{
    gcp_catalog::Zone,
    picker::{Job, Next, Opt, Screen},
};
use std::rc::Rc;

pub fn area(zone: &str) -> String {
    let prefix = crate::gcp_geo::region(zone)
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
#[derive(Clone)]
pub(crate) struct Presentation {
    pub preferred: Option<String>,
    pub label: Rc<dyn Fn(&Zone) -> String>,
    pub prices: Option<Job<()>>,
}
impl Default for Presentation {
    fn default() -> Self {
        Self {
            preferred: None,
            label: Rc::new(|z| z.name.clone()),
            prices: None,
        }
    }
}
fn options(zones: Vec<Zone>, display: &Presentation) -> Vec<Opt<String>> {
    zones
        .into_iter()
        .map(|z| {
            let label = (display.label)(&z);
            Opt::new(z.name, label)
        })
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
                groups(&zones)
                    .into_iter()
                    .map(|(name, count)| {
                        Opt::new(
                            format!("@area:{name}"),
                            format!(
                                "{name} ({count} {})",
                                if count == 1 { "zone" } else { "zones" }
                            ),
                        )
                    })
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
mod tests {
    use super::*;
    use crate::picker::{testing::*, Outcome};
    #[test]
    fn groups_preserve_distance_counts_filter_zones_and_back_stack() {
        let zones = [
            "us-west4-a",
            "us-west1-c",
            "us-east5-b",
            "us-east1-d",
            "asia-south1-b",
        ]
        .into_iter()
        .map(|s| Zone {
            name: s.into(),
            types: Vec::new(),
        })
        .collect::<Vec<_>>();
        let job = Job::new({
            let zones = zones.clone();
            move || Ok(zones.clone())
        });
        let result = drive(screen(job, zones, "TPU zone", "", Next::Done), |keys| {
            keys.wait(|s| s.labels.len() == 3);
            assert_eq!(
                keys.labels(),
                [
                    "us-west (2 zones)",
                    "us-east (2 zones)",
                    "asia-south (1 zone)"
                ]
            );
            keys.press_until(RIGHT, on("us-west zones"));
            assert_eq!(keys.labels(), ["us-west4-a", "us-west1-c"]);
            keys.press_until(BACK, on("TPU zone"));
            keys.type_text("us-east1-d");
            assert_eq!(keys.labels(), ["us-east1-d"]);
            keys.press(RIGHT);
        });
        assert_eq!(result, Outcome::Done("us-east1-d".into()));
    }
    #[test]
    fn preferred_zone_is_highlighted_without_changing_distance_order() {
        let zones = ["us-west4-a", "us-west1-c", "us-east5-b"]
            .into_iter()
            .map(|s| Zone {
                name: s.into(),
                types: Vec::new(),
            })
            .collect::<Vec<_>>();
        let job = Job::new({
            let zones = zones.clone();
            move || Ok(zones.clone())
        });
        let result = drive(
            present(
                job,
                zones,
                "TPU zone",
                "",
                Presentation {
                    preferred: Some("us-west1-c".into()),
                    ..Presentation::default()
                },
                Next::Done,
            ),
            |keys| {
                keys.press_until(RIGHT, on("us-west zones"));
                assert_eq!(keys.labels(), ["us-west4-a", "us-west1-c"]);
                assert!(keys
                    .snapshot()
                    .body
                    .iter()
                    .any(|line| line.starts_with('›') && line.contains("us-west1-c")));
                keys.press(ENTER);
            },
        );
        assert_eq!(result, Outcome::Done("us-west1-c".into()));
    }
    #[test]
    fn area_names_come_from_region_names() {
        for (zone, wanted) in [
            ("us-east5-b", "us-east"),
            ("northamerica-northeast2-a", "northamerica-northeast"),
            ("southamerica-west1-c", "southamerica"),
            ("southamerica-east1-c", "southamerica"),
            ("new-place17-ai1b", "new-place"),
        ] {
            assert_eq!(area(zone), wanted);
        }
    }
}
