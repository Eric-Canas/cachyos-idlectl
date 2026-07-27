//! `local_service_busy`: a long-running local service that is *up* is not a long-running
//! local service that is *in use*.
//!
//! # The measurement this fact exists for
//!
//! [FACT-32]. A model server left running "just in case" turned a start-up decision into a
//! permanent block. The machine never slept again all night, and roughly 12 GiB of video
//! memory stayed pinned — which is exactly the memory a game needs. "The unit is active"
//! is not a usable signal, and neither is a health endpoint that returns OK whenever the
//! process is alive ([FACT-33]).
//!
//! # Why cumulative counters and not instantaneous state
//!
//! [FACT-31]. A request that begins and ends between two samples is invisible to
//! instantaneous state. Counters are monotonic, so any request between two samples leaves
//! a permanent trace — the sampling interval stops mattering, which is what makes an
//! event-driven daemon able to use this at all.
//!
//! # Why it ships disabled
//!
//! [FACT-44], and it is [FACT-34] read in the other direction. If the counters cannot be
//! read the fact is `INDETERMINATE`, never `FALSE` — a permissive default would silently
//! disable this veto for everybody who had not changed their service's configuration,
//! which on day one is everybody. But the counter endpoint of the service this was written
//! for is **off by default**, so a detector that shipped enabled would be unreadable, and
//! therefore indeterminate, on every machine the day it was installed. Combined with the
//! block-independent doubt veto of [FACT-4], that would freeze every installation. Setting
//! `counters_url` is what enables it; until then it is `UNAVAILABLE` and its detector does
//! not run.

use std::time::Duration;

use idlectl_policy::{BootInstant, FactId};

use super::{Context, Reading, ago};

/// The default idle window: thirty minutes with no counter movement.
const DEFAULT_IDLE_WINDOW: Duration = Duration::from_secs(1800);

/// How long to wait for the endpoint before calling it unreadable.
///
/// Short on purpose. This is a loopback request to a service on the same machine; if it
/// has not answered in three seconds it is wedged, and a wedged service is exactly the
/// case that must produce doubt rather than a hang inside the decision loop.
const TIMEOUT: Duration = Duration::from_secs(3);

/// Remembers the previous sample.
///
/// In memory rather than on disk, which satisfies [FACT-36]: it survives suspend, because
/// suspending is not using the service, and it is gone after a cold boot, because a cold
/// boot has no history. A daemon restart also clears it, and that lands on [FACT-35] —
/// the first sample after start is treated as "in use", so the idle countdown restarts
/// rather than the service being assumed idle since forever.
#[derive(Debug, Default)]
pub struct Sampler {
    previous: Option<u64>,
    /// When a counter last moved.
    last_movement: Option<BootInstant>,
}

impl Sampler {
    pub async fn sample(&mut self, ctx: &Context<'_>) -> Reading {
        let settings = ctx.policy.fact_settings(FactId::LocalServiceBusy);
        let Some(url) = settings.counters_url.clone() else {
            return Reading::absent("no counters_url is configured");
        };
        let window = settings.idle_window.unwrap_or(DEFAULT_IDLE_WINDOW);
        let now = crate::clock::now();

        let body = match fetch(&url).await {
            Ok(body) => body,
            // [FACT-34]. Not FALSE. The service being unreachable is precisely when the
            // daemon cannot tell whether work is in flight.
            Err(err) => return Reading::doubt(format!("{url} could not be read: {err}")),
        };

        let Some(reading) = parse(&body) else {
            return Reading::doubt(format!(
                "{url} exposed no counters this detector understands"
            ));
        };

        let moved = match self.previous {
            // [FACT-35]: at the first sample nothing is known about the past. The idle
            // countdown starts now rather than assuming the service has been idle forever.
            None => true,
            Some(previous) => reading.counters != previous,
        };
        self.previous = Some(reading.counters);

        if moved || reading.in_flight > 0 {
            self.last_movement = Some(now);
        }
        let since = self.last_movement.unwrap_or(now);
        let idle_for = now.since(since);

        if reading.in_flight > 0 {
            return Reading::yes(format!("{} request(s) in flight", reading.in_flight));
        }
        if idle_for < window {
            Reading::yes(format!(
                "counters last moved {} (idle window {}s)",
                ago(idle_for),
                window.as_secs()
            ))
        } else {
            Reading::no(format!(
                "no counter has moved for {} (idle window {}s)",
                ago(idle_for),
                window.as_secs()
            ))
        }
    }
}

struct Sample {
    /// The sum of every cumulative counter. Only its *change* is meaningful; the absolute
    /// value is a fingerprint, not a measurement.
    counters: u64,
    /// Work the service says it is doing right now.
    in_flight: u64,
}

/// Gauges that mean "work is in flight", by name.
///
/// A heuristic, and labelled as one. The normative signal is [FACT-30]'s counter movement;
/// this only shortens the reaction time for a request that started between two samples and
/// has not finished. Getting it wrong in either direction costs at most one sampling
/// interval of accuracy, never a wrong verdict, because the counter path decides on its own.
const IN_FLIGHT_HINTS: [&str; 4] = ["processing", "in_flight", "inflight", "active_requests"];

/// Parses the Prometheus text exposition format.
///
/// The `# TYPE <name> counter` declarations are what makes this generic: they are the
/// service's own statement about which of its metrics are cumulative, so the detector does
/// not have to guess from names. When a body carries no `# TYPE` lines at all, every
/// numeric sample is summed instead — cruder, but a body of bare `name value` pairs is
/// still monotonic in practice and the alternative is refusing to work with it.
fn parse(body: &str) -> Option<Sample> {
    let mut counter_names: Vec<&str> = Vec::new();
    let mut gauge_names: Vec<&str> = Vec::new();
    for line in body.lines() {
        let Some(rest) = line.strip_prefix("# TYPE ") else {
            continue;
        };
        let mut parts = rest.split_whitespace();
        let (Some(name), Some(kind)) = (parts.next(), parts.next()) else {
            continue;
        };
        match kind {
            "counter" => counter_names.push(name),
            "gauge" => gauge_names.push(name),
            _ => {}
        }
    }
    let typed = !counter_names.is_empty() || !gauge_names.is_empty();

    let mut counters: u64 = 0;
    let mut in_flight: u64 = 0;
    let mut seen_any = false;

    for line in body.lines() {
        if line.starts_with('#') {
            continue;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // `name{label="v",...} value [timestamp]`. Splitting on the last whitespace run
        // would take the timestamp; splitting on the first takes the value, and labels
        // never contain unquoted whitespace in the exposition format.
        let (key, value) = match line.rsplit_once(' ') {
            Some((k, v)) => (k, v),
            None => continue,
        };
        let Ok(value) = value.parse::<f64>() else {
            continue;
        };
        if !value.is_finite() || value < 0.0 {
            continue;
        }
        let name = key.split('{').next().unwrap_or(key).trim();
        seen_any = true;

        // Truncating to whole units is deliberate. These are compared for *equality*
        // between samples, and a float sum of thousands of values is not stable enough to
        // compare that way -- adding the same numbers in a different order can differ in
        // the last bits and would read as movement on a completely idle service.
        let whole = value as u64;

        if !typed {
            counters = counters.saturating_add(whole);
            continue;
        }
        if counter_names.contains(&name) {
            counters = counters.saturating_add(whole);
        } else if gauge_names.contains(&name) && IN_FLIGHT_HINTS.iter().any(|h| name.contains(h)) {
            in_flight = in_flight.saturating_add(whole);
        }
    }

    seen_any.then_some(Sample {
        counters,
        in_flight,
    })
}

/// A single plain-HTTP GET.
///
/// Deliberately hand-written and deliberately plaintext-only. This endpoint is on the
/// loopback interface of the same machine by construction — it is the service the daemon
/// is deciding about — so a TLS stack would add a large dependency tree, a certificate
/// trust decision and an attack surface to fetch a page from `127.0.0.1`. An `https://`
/// URL is refused with a message that says so rather than silently failing.
async fn fetch(url: &str) -> Result<String, String> {
    let rest = url.strip_prefix("http://").ok_or_else(|| {
        if url.starts_with("https://") {
            "https is not supported: counters_url is expected to be a loopback endpoint".to_owned()
        } else {
            format!("not an http:// URL: {url}")
        }
    })?;

    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let host = authority.rsplit_once(':').map_or(authority, |(h, _)| h);
    let addr = if authority.contains(':') {
        authority.to_owned()
    } else {
        format!("{authority}:80")
    };

    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: idlepolicyd\r\nAccept: text/plain\r\nConnection: close\r\n\r\n"
    );

    let work = async {
        use futures_lite::io::{AsyncReadExt, AsyncWriteExt};

        let mut stream = async_net::TcpStream::connect(&addr)
            .await
            .map_err(|e| e.to_string())?;
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|e| e.to_string())?;

        // Bounded. A service that streams megabytes at this endpoint is misconfigured, and
        // reading it all into the daemon that decides whether the machine may sleep is not
        // a failure mode worth having.
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let n = stream.read(&mut chunk).await.map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.len() > 4 * 1024 * 1024 {
                return Err("response exceeded 4 MiB".to_owned());
            }
        }
        Ok(String::from_utf8_lossy(&buf).into_owned())
    };

    let timeout = async {
        async_io::Timer::after(TIMEOUT).await;
        Err(format!("no answer in {}s", TIMEOUT.as_secs()))
    };

    let response = futures_lite::future::or(work, timeout).await?;

    let (head, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| "malformed HTTP response".to_owned())?;
    let status = head.lines().next().unwrap_or_default();
    if !status.contains(" 200") {
        return Err(format!("HTTP status: {status}"));
    }
    Ok(body.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str = "\
# HELP svc_tokens_total Tokens processed.
# TYPE svc_tokens_total counter
svc_tokens_total 1200
# TYPE svc_requests_processing gauge
svc_requests_processing 0
# TYPE svc_queue_depth gauge
svc_queue_depth 7
";

    #[test]
    fn only_declared_counters_are_summed() {
        let sample = parse(BODY).expect("counters present");
        // 1200 from the counter. The two gauges must not be added: a gauge that goes up
        // and down would read as movement every time it moved in either direction.
        assert_eq!(sample.counters, 1200);
        assert_eq!(sample.in_flight, 0);
    }

    #[test]
    fn an_in_flight_gauge_is_recognised_by_name() {
        let body = BODY.replace("svc_requests_processing 0", "svc_requests_processing 2");
        let sample = parse(&body).expect("counters present");
        assert_eq!(sample.in_flight, 2);
        // ...and a gauge whose name carries no hint still does not count as in flight.
        assert_eq!(sample.counters, 1200);
    }

    #[test]
    fn labels_and_timestamps_do_not_derail_the_value() {
        let body = "# TYPE a_total counter\na_total{code=\"200\",path=\"/v1\"} 42\n";
        assert_eq!(parse(body).expect("parsed").counters, 42);
    }

    #[test]
    fn an_untyped_body_sums_every_numeric_sample() {
        let sample = parse("alpha 3\nbeta 4\n").expect("parsed");
        assert_eq!(sample.counters, 7);
    }

    #[test]
    fn a_body_with_no_samples_is_not_a_reading() {
        // The caller turns this into doubt rather than into "idle": an endpoint that
        // answers 200 with nothing in it tells us nothing about the service.
        assert!(parse("# HELP only comments\n").is_none());
        assert!(parse("").is_none());
    }

    #[test]
    fn https_is_refused_with_a_reason() {
        let err = futures_lite::future::block_on(fetch("https://localhost/metrics")).unwrap_err();
        assert!(err.contains("https is not supported"), "{err}");
    }

    #[test]
    fn a_non_http_url_is_refused() {
        let err = futures_lite::future::block_on(fetch("/var/run/thing.sock")).unwrap_err();
        assert!(err.contains("not an http:// URL"), "{err}");
    }
}
