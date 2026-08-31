//! Listener counts for a direct Icecast service.
//!
//! Audio Pub tells us how many people are listening over the live-events feed
//! (`sse::LiveEvent::Listeners`). A plain Icecast mount has no such channel: the
//! source connection carries audio in one direction and the server never
//! volunteers anything, so the only thing the protocol offers is the status
//! document, read on a timer. That is why this is polled while everything else
//! in `net` is event-driven — see [`super::LISTENER_POLL_INTERVAL`].
//!
//! The interesting half is not the reading but the *addressing*. **The mount a
//! source publishes to is very often not the mount anyone listens to**: a
//! Liquidsoap (or Icecast relay, or `ices`) instance consumes the raw mount and
//! republishes a processed one, so counting listeners on the mount we hold
//! reports zero forever while the station is busy. [`stats_target`] is what
//! resolves the user's "listener count URL" field — a whole URL, a `host/mount`,
//! or a bare mount name — into the status document to read and the mount to
//! count in it.

use serde_json::Value;

/// Icecast's JSON status document, served at the root of every stock install
/// since 2.4 and by the compatible servers (RSAS among them). There is no
/// configuration for this path, which is why it is a constant rather than
/// another field for the user to fill in.
const STATUS_PATH: &str = "/status-json.xsl";

/// Where to ask for listener counts, and which mount to count once there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatsTarget {
    /// Absolute URL of the status document.
    pub status_url: String,
    /// The mount to count, with its leading slash, or `None` to count every
    /// mount the server reports. `None` is what a listener URL naming only a
    /// server means: "how many people are listening to my station", summed
    /// across whatever it is publishing.
    pub mount: Option<String>,
}

/// What one reading of a status document said.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counts {
    pub listeners: u32,
    /// The server's own high-water mark, when it publishes one. Stock Icecast's
    /// *public* document does not, so this is usually zero and the caller's own
    /// high-water mark is what the UI ends up showing.
    pub peak: u32,
    /// Whether the mount we asked about was in the document at all.
    ///
    /// Separate from `listeners == 0` because the two have different causes and
    /// only one of them is a mistake: a mount with nobody on it reports zero,
    /// and so does a mount whose name was mistyped. See
    /// [`super::spawn_listener_poll`], which says so in the log once the
    /// difference has lasted long enough to be worth mentioning.
    pub matched: bool,
}

/// Normalizes a mount to the leading-slash form Icecast reports in `mount`.
fn with_leading_slash(mount: &str) -> String {
    format!("/{}", mount.trim().trim_start_matches('/'))
}

/// The configured Icecast endpoint, as the fallback for a listener URL that
/// names no server of its own.
///
/// Parsed rather than concatenated for the reason
/// [`super::icecast::split_host_port`] exists: the server field routinely holds
/// a `host:port` that the port field must not be appended to a second time.
fn configured_endpoint(server: &str, port: u16) -> Result<(String, u16), String> {
    let (host, embedded) = super::icecast::split_host_port(server)?;
    let port = embedded.unwrap_or(port);
    if port == 0 {
        return Err("Enter a valid Icecast port.".to_string());
    }
    Ok((host, port))
}

/// Pulls Icecast's own `?mount=` filter out of a query string, so a status URL
/// copied from the server's admin pages keeps meaning what it meant there.
fn mount_in_query(query: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        if !key.eq_ignore_ascii_case("mount") {
            return None;
        }
        let decoded = urlencoding::decode(value).ok()?.into_owned();
        if decoded.trim().is_empty() {
            None
        } else {
            Some(decoded)
        }
    })
}

/// Resolves the "listener count URL" field into something that can be fetched.
///
/// `listener_url` is deliberately forgiving, because what a user has to hand is
/// whatever their host wrote down for them. All of these resolve:
///
/// - **empty** — count the mount we publish to, on the service's own server.
///   The behaviour anyone who has not touched the field should get.
/// - `stream.mp3` — a bare mount name on the service's own server. No slash and
///   no scheme, which is the discriminator: a value with neither is a mount, not
///   a host.
/// - `/stream.mp3` — the same thing written as a path.
/// - `radio.example.com/stream.mp3`, `radio.example.com:8000/stream.mp3` — a
///   mount on a named server, for a relay that lives somewhere else.
/// - `http://radio.example.com:8000/stream.mp3` — the listen URL, verbatim, as
///   pasted out of a player.
/// - `http://radio.example.com:8000/` — a server and no mount: everyone on it.
/// - `http://radio.example.com:8000/status-json.xsl?mount=/stream.mp3` — the
///   status document itself, which is kept whole so Icecast's own filter still
///   applies.
///
/// A server named without a port is reached on the scheme's port (80, or 443 for
/// `https`) — **except** when it is this service's own Icecast server, where the
/// port sitting beside it in the dialog is plainly what was meant.
pub fn stats_target(
    server: &str,
    port: u16,
    source_mount: &str,
    listener_url: &str,
) -> Result<StatsTarget, String> {
    let resolved = resolve(server, port, source_mount, listener_url)?;
    Ok(StatsTarget {
        status_url: format!(
            "{}://{}:{}{}",
            resolved.scheme,
            resolved.host,
            resolved.port,
            resolved.status_path.as_deref().unwrap_or(STATUS_PATH)
        ),
        mount: resolved.mount,
    })
}

/// The address a listener tunes in at, for a direct Icecast service.
///
/// The same field, read for the other thing it says. `listener_url` names where
/// the audience is; a status document is what [`stats_target`] wants from that
/// and a listen URL is what the Home tab's "Go to stream page" and a Mastodon
/// `{url}` want, so both are rendered from one [`resolve`] rather than from two
/// parsers that would drift.
///
/// The mount matters more here than it does for counting. A field naming only a
/// server — "count everyone on my relay" — leaves the audience's mount genuinely
/// unknown, so this falls back on the mount we publish to, on the server we
/// publish to: not where the relay's listeners are, but a real address that
/// really plays this stream, which beats having nothing to post. A default port
/// for the scheme is left off, because `http://radio.example.com:80/live.mp3` is
/// a URL nobody would write down.
pub fn listen_url(
    server: &str,
    port: u16,
    source_mount: &str,
    listener_url: &str,
) -> Result<String, String> {
    let resolved = resolve(server, port, source_mount, listener_url)?;
    let mount = match resolved.mount {
        Some(mount) => mount,
        None => {
            let source_mount = source_mount.trim();
            if source_mount.is_empty() {
                return Err(
                    "Pubsplash does not know which mount listeners are on.".to_string()
                );
            }
            with_leading_slash(source_mount)
        }
    };
    let default_port = if resolved.scheme == "https" { 443 } else { 80 };
    let authority = if resolved.port == default_port {
        resolved.host
    } else {
        format!("{}:{}", resolved.host, resolved.port)
    };
    Ok(format!("{}://{authority}{mount}", resolved.scheme))
}

/// The listener count URL field, resolved to an endpoint and a mount.
///
/// One parse, two renderings — see [`listen_url`].
struct Resolved {
    /// `http` or `https`.
    scheme: String,
    host: String,
    port: u16,
    /// The mount, with its leading slash, or `None` when the field named a whole
    /// server rather than one mount.
    mount: Option<String>,
    /// Set only when the field pointed straight at a status document: the path
    /// and query to fetch verbatim, so a URL that already filters goes on
    /// filtering. Meaningless to a listen URL, which uses `mount`.
    status_path: Option<String>,
}

fn resolve(
    server: &str,
    port: u16,
    source_mount: &str,
    listener_url: &str,
) -> Result<Resolved, String> {
    let field = listener_url.trim();
    if field.is_empty() {
        let (host, port) = configured_endpoint(server, port)?;
        let source_mount = source_mount.trim();
        return Ok(Resolved {
            scheme: "http".to_string(),
            host,
            port,
            // The root mount is a real mount, spelled `/` by
            // `super::normalize_mount` and reported as `/` by Icecast, so it
            // has to survive the leading-slash trim rather than reading as no
            // mount at all. An *empty* source mount is refused long before this
            // by `direct_icecast_target`; counting the whole server is the
            // honest answer for the case that cannot arrive.
            mount: (!source_mount.is_empty()).then(|| with_leading_slash(source_mount)),
            status_path: None,
        });
    }

    let (scheme, rest) = match field.split_once("://") {
        Some((scheme, rest)) => {
            let scheme = scheme.trim().to_ascii_lowercase();
            if scheme != "http" && scheme != "https" {
                return Err(format!(
                    "The listener count URL {field:?} is not an http or https address."
                ));
            }
            (Some(scheme), rest)
        }
        None => (None, field),
    };
    // Credentials in the authority are never ours to use, exactly as in
    // `split_host_port`: the status document is public, and a password pasted in
    // here would go out in a request we never asked the user to authorize.
    let rest = match rest.rsplit_once('@') {
        Some((_, after)) => after,
        None => rest,
    };
    // The discriminator for the bare-mount form. `stream.mp3` has a dot in it
    // and would otherwise read as a hostname, which is how this field is most
    // likely to be filled in by hand.
    let (authority, path) = if scheme.is_none() && !rest.contains('/') {
        ("", rest)
    } else {
        match rest.find('/') {
            Some(cut) => (&rest[..cut], &rest[cut..]),
            None => (rest, ""),
        }
    };

    let https = scheme.as_deref() == Some("https");
    let scheme = scheme.unwrap_or_else(|| "http".to_string());
    let (host, port) = if authority.trim().is_empty() {
        configured_endpoint(server, port)?
    } else {
        let (host, typed) = super::icecast::split_host_port(authority)
            .map_err(|e| format!("The listener count URL is not usable: {e}"))?;
        let port = match typed {
            Some(typed) => typed,
            // The same server as the source, written out again. The port field
            // is right there and saying 80 instead would be perverse.
            None if configured_endpoint(server, port)
                .is_ok_and(|(configured, _)| configured.eq_ignore_ascii_case(&host)) =>
            {
                configured_endpoint(server, port)?.1
            }
            None if https => 443,
            None => 80,
        };
        (host, port)
    };

    let (path, query) = match path.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (path, None),
    };
    let trimmed = path.trim_end_matches('/');
    let file = trimmed.rsplit('/').next().unwrap_or("");
    if file.eq_ignore_ascii_case("status-json.xsl") {
        // Pointed straight at a status document: keep it as given, query
        // included, so a URL that already filters goes on filtering.
        let suffix = match query {
            Some(query) => format!("{path}?{query}"),
            None => path.to_string(),
        };
        return Ok(Resolved {
            scheme,
            host,
            port,
            mount: query
                .and_then(mount_in_query)
                .map(|m| with_leading_slash(&m)),
            status_path: Some(suffix),
        });
    }
    Ok(Resolved {
        scheme,
        host,
        port,
        mount: (!trimmed.is_empty()).then(|| with_leading_slash(trimmed)),
        status_path: None,
    })
}

/// Reads a number that a status document may write as a JSON number or, on some
/// servers, as a string.
fn number(value: Option<&Value>) -> u32 {
    match value {
        Some(Value::Number(n)) => n
            .as_u64()
            .or_else(|| n.as_f64().map(|f| f.max(0.0) as u64))
            .unwrap_or(0)
            .min(u32::MAX as u64) as u32,
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

/// Counts listeners in one status document.
///
/// `mount` is `None` to count every mount the server reports.
///
/// **`icestats.source` is an array of mounts, or a single mount object, or
/// absent.** That is not a quirk of one server: Icecast's status document is
/// generated from the same XSLT-ish serializer as its XML, which has no way to
/// say "a list of one", so a server with exactly one mount connected emits a
/// bare object and a server with none emits nothing at all. Handling only the
/// array is how this reports zero on precisely the station that is easiest to
/// get right.
pub fn parse_counts(body: &str, mount: Option<&str>) -> Result<Counts, String> {
    let value: Value =
        serde_json::from_str(body).map_err(|e| format!("the status document is not JSON ({e})"))?;
    let Some(stats) = value.get("icestats") else {
        return Err("the status document has no icestats object".to_string());
    };
    let sources: Vec<&Value> = match stats.get("source") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.iter().collect(),
        Some(one) => vec![one],
    };

    let wanted = mount.map(with_leading_slash);
    let mut counts = Counts {
        // Nothing to match when every mount counts, so the question does not
        // apply and the answer must not read as a misconfiguration.
        matched: wanted.is_none(),
        ..Counts::default()
    };
    for source in sources {
        if let Some(wanted) = &wanted {
            let Some(actual) = source.get("mount").and_then(Value::as_str) else {
                continue;
            };
            // Mount names are case-sensitive to Icecast itself, so they are
            // compared that way here.
            if &with_leading_slash(actual) != wanted {
                continue;
            }
        }
        counts.matched = true;
        counts.listeners = counts
            .listeners
            .saturating_add(number(source.get("listeners")));
        counts.peak = counts
            .peak
            .saturating_add(number(source.get("listener_peak")));
    }
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The document the feature was built against: a real RSAS server relaying
    /// three mounts, none of which is the mount its source client publishes to.
    const REAL_STATUS: &str = r#"{
        "icestats": {
            "admin": "",
            "host": "oriolgomez.com",
            "server_id": "RSAS",
            "source": [
                {
                    "ice-bitrate": 160,
                    "listeners": 2,
                    "listenurl": "http://oriolgomez.com/beats.mp3",
                    "mount": "/beats.mp3",
                    "server_type": "audio/mpeg"
                },
                {
                    "ice-bitrate": 160,
                    "listeners": 7,
                    "listenurl": "http://oriolgomez.com/stream.mp3",
                    "mount": "/stream.mp3",
                    "server_type": "audio/mpeg"
                },
                {
                    "ice-bitrate": 160,
                    "listeners": 1,
                    "listenurl": "http://oriolgomez.com/suno.mp3",
                    "mount": "/suno.mp3",
                    "server_type": "audio/mpeg"
                }
            ]
        }
    }"#;

    /// The same field, read for the address rather than for the count. Every
    /// form `stats_target` documents is walked here, because the two renderings
    /// share one parser and this is what keeps the sharing honest.
    #[test]
    fn the_listen_address_is_built_from_the_same_forms() {
        let listen = |field: &str| listen_url("radio.example.com", 8000, "/live.mp3", field);

        // Empty: the endpoint we publish to, plus the mount we publish to.
        assert_eq!(listen("").unwrap(), "http://radio.example.com:8000/live.mp3");
        // A bare mount name, and the same thing written as a path: the relay's
        // mount on our own server.
        assert_eq!(
            listen("stream.mp3").unwrap(),
            "http://radio.example.com:8000/stream.mp3"
        );
        assert_eq!(
            listen("/stream.mp3").unwrap(),
            "http://radio.example.com:8000/stream.mp3"
        );
        // A relay elsewhere, with and without a port of its own.
        assert_eq!(
            listen("relay.example.com:8010/stream.mp3").unwrap(),
            "http://relay.example.com:8010/stream.mp3"
        );
        assert_eq!(
            listen("http://relay.example.com/stream.mp3").unwrap(),
            "http://relay.example.com/stream.mp3"
        );
        // The scheme's own port is left off; any other port is kept.
        assert_eq!(
            listen("https://listen.example.com:443/stream.mp3").unwrap(),
            "https://listen.example.com/stream.mp3"
        );
        assert_eq!(
            listen("https://listen.example.com:8443/stream.mp3").unwrap(),
            "https://listen.example.com:8443/stream.mp3"
        );
        // Our own server written out again keeps the port from the dialog, the
        // same rule the status URL follows.
        assert_eq!(
            listen("radio.example.com/stream.mp3").unwrap(),
            "http://radio.example.com:8000/stream.mp3"
        );
        // A server and no mount names no one stream, so the mount we publish to
        // stands in — a real address on the server they named.
        assert_eq!(
            listen("http://relay.example.com:8000/").unwrap(),
            "http://relay.example.com:8000/live.mp3"
        );
        // A status document that filters names its mount, and that is the one.
        assert_eq!(
            listen("http://relay.example.com:8000/status-json.xsl?mount=/stream.mp3").unwrap(),
            "http://relay.example.com:8000/stream.mp3"
        );
        // Credentials never survive into an address we hand out.
        assert_eq!(
            listen("http://user:pw@relay.example.com:8000/stream.mp3").unwrap(),
            "http://relay.example.com:8000/stream.mp3"
        );
        // No server at all, and no mount at all, are the two ways to have no
        // address rather than a wrong one.
        assert!(listen_url("", 8000, "/live.mp3", "").is_err());
        assert!(listen_url("radio.example.com", 8000, "", "").is_err());
    }

    #[test]
    fn an_empty_field_counts_the_mount_we_publish_to() {
        let target = stats_target("radio.oriolgomez.com", 8000, "live.mp3", "").unwrap();
        assert_eq!(
            target,
            StatsTarget {
                status_url: "http://radio.oriolgomez.com:8000/status-json.xsl".into(),
                mount: Some("/live.mp3".into()),
            }
        );
    }

    /// The whole point of the field: the source publishes `live.mp3` and a
    /// Liquidsoap relay republishes `stream.mp3`, which is where everyone is.
    #[test]
    fn a_bare_mount_name_stays_on_the_services_own_server() {
        let target = stats_target("radio.oriolgomez.com", 8000, "live.mp3", "stream.mp3").unwrap();
        assert_eq!(
            target,
            StatsTarget {
                status_url: "http://radio.oriolgomez.com:8000/status-json.xsl".into(),
                mount: Some("/stream.mp3".into()),
            }
        );
        // ...and written as a path, which is the same thing.
        assert_eq!(
            stats_target("radio.oriolgomez.com", 8000, "live.mp3", "/stream.mp3").unwrap(),
            target
        );
    }

    /// A `host:port` typed into the server field must not have the port field
    /// appended a second time here either.
    #[test]
    fn a_server_field_carrying_its_own_port_is_not_doubled() {
        let target =
            stats_target("radio.oriolgomez.com:8000", 8000, "live.mp3", "stream.mp3").unwrap();
        assert_eq!(
            target.status_url,
            "http://radio.oriolgomez.com:8000/status-json.xsl"
        );
    }

    #[test]
    fn a_whole_listen_url_is_understood() {
        let target = stats_target(
            "radio.oriolgomez.com",
            8000,
            "live.mp3",
            "http://radio.oriolgomez.com:8000/stream.mp3",
        )
        .unwrap();
        assert_eq!(
            target,
            StatsTarget {
                status_url: "http://radio.oriolgomez.com:8000/status-json.xsl".into(),
                mount: Some("/stream.mp3".into()),
            }
        );
    }

    /// A relay on another host, named without a port, is reached on the
    /// scheme's port -- 8000 belongs to the source server, not to this one.
    #[test]
    fn another_host_without_a_port_uses_the_schemes_port() {
        assert_eq!(
            stats_target(
                "radio.example.com",
                8000,
                "live.mp3",
                "https://cdn.example.net/live"
            )
            .unwrap()
            .status_url,
            "https://cdn.example.net:443/status-json.xsl"
        );
        assert_eq!(
            stats_target(
                "radio.example.com",
                8000,
                "live.mp3",
                "cdn.example.net/live"
            )
            .unwrap()
            .status_url,
            "http://cdn.example.net:80/status-json.xsl"
        );
    }

    /// ...but the service's *own* server written out again keeps the port that
    /// is sitting right beside it in the dialog.
    #[test]
    fn the_same_host_without_a_port_keeps_the_configured_one() {
        assert_eq!(
            stats_target(
                "radio.oriolgomez.com",
                8000,
                "live.mp3",
                "radio.oriolgomez.com/stream.mp3",
            )
            .unwrap()
            .status_url,
            "http://radio.oriolgomez.com:8000/status-json.xsl"
        );
    }

    /// A source publishing to the server root counts the root, not the whole
    /// server: `direct_icecast_target` accepts `/` as a mount, and Icecast
    /// reports it as one.
    #[test]
    fn the_root_mount_is_a_mount() {
        assert_eq!(
            stats_target("radio.example.com", 8000, "/", "")
                .unwrap()
                .mount,
            Some("/".into())
        );
        let body = r#"{"icestats":{"source":[{"mount":"/","listeners":6},
            {"mount":"/other","listeners":9}]}}"#;
        assert_eq!(parse_counts(body, Some("/")).unwrap().listeners, 6);
    }

    #[test]
    fn a_server_with_no_mount_counts_everyone_on_it() {
        for field in [
            "http://radio.example.com:8000",
            "http://radio.example.com:8000/",
        ] {
            let target = stats_target("radio.example.com", 8000, "live.mp3", field).unwrap();
            assert_eq!(target.mount, None, "{field}");
            assert_eq!(
                target.status_url,
                "http://radio.example.com:8000/status-json.xsl"
            );
        }
    }

    #[test]
    fn a_status_url_is_kept_whole_and_its_own_filter_respected() {
        let target = stats_target(
            "radio.example.com",
            8000,
            "live.mp3",
            "http://radio.example.com:8000/status-json.xsl?mount=/stream.mp3",
        )
        .unwrap();
        assert_eq!(
            target,
            StatsTarget {
                status_url: "http://radio.example.com:8000/status-json.xsl?mount=/stream.mp3"
                    .into(),
                mount: Some("/stream.mp3".into()),
            }
        );
        // Percent-encoded, as a browser would have written it.
        assert_eq!(
            stats_target(
                "radio.example.com",
                8000,
                "live.mp3",
                "http://radio.example.com:8000/status-json.xsl?mount=%2Fstream.mp3",
            )
            .unwrap()
            .mount,
            Some("/stream.mp3".into())
        );
        // Unfiltered: the whole server.
        assert_eq!(
            stats_target(
                "radio.example.com",
                8000,
                "live.mp3",
                "http://radio.example.com:8000/status-json.xsl",
            )
            .unwrap()
            .mount,
            None
        );
    }

    #[test]
    fn credentials_in_the_authority_are_dropped() {
        assert_eq!(
            stats_target(
                "radio.example.com",
                8000,
                "live.mp3",
                "http://source:hunter2@radio.example.com:8000/stream.mp3",
            )
            .unwrap()
            .status_url,
            "http://radio.example.com:8000/status-json.xsl"
        );
    }

    #[test]
    fn a_scheme_we_cannot_fetch_is_refused_rather_than_guessed_at() {
        let error = stats_target(
            "radio.example.com",
            8000,
            "live.mp3",
            "icy://radio/stream.mp3",
        )
        .unwrap_err();
        assert!(error.contains("http"), "{error}");
    }

    #[test]
    fn the_named_mount_is_the_only_one_counted() {
        let counts = parse_counts(REAL_STATUS, Some("/stream.mp3")).unwrap();
        assert_eq!(counts.listeners, 7);
        assert!(counts.matched);
        // Written without the slash, which is how the dialog's field takes it.
        assert_eq!(
            parse_counts(REAL_STATUS, Some("stream.mp3"))
                .unwrap()
                .listeners,
            7
        );
    }

    #[test]
    fn no_mount_named_sums_the_server() {
        let counts = parse_counts(REAL_STATUS, None).unwrap();
        assert_eq!(counts.listeners, 10);
        assert!(counts.matched);
    }

    /// The mount we publish to is not in this document at all, which is exactly
    /// the situation the feature exists for -- and is reported as *missing*
    /// rather than as an idle mount, because the two want different answers.
    #[test]
    fn a_mount_that_is_not_up_is_missing_rather_than_idle() {
        let counts = parse_counts(REAL_STATUS, Some("/live.mp3")).unwrap();
        assert_eq!(counts.listeners, 0);
        assert!(!counts.matched);
    }

    /// One mount connected: Icecast writes a bare object where it writes an
    /// array for two. Reading only the array form reports zero on the simplest
    /// possible station.
    #[test]
    fn a_single_mount_is_an_object_not_an_array() {
        let body = r#"{"icestats":{"source":{"mount":"/stream.mp3","listeners":4}}}"#;
        assert_eq!(
            parse_counts(body, Some("/stream.mp3")).unwrap().listeners,
            4
        );
        assert_eq!(parse_counts(body, None).unwrap().listeners, 4);
    }

    /// No mount connected at all: `source` is absent, which is a server with
    /// nobody on it and not a broken document.
    #[test]
    fn a_server_with_no_mounts_counts_zero() {
        let counts = parse_counts(r#"{"icestats":{"server_id":"Icecast 2.4.4"}}"#, None).unwrap();
        assert_eq!(
            counts,
            Counts {
                listeners: 0,
                peak: 0,
                matched: true
            }
        );
    }

    #[test]
    fn a_published_peak_is_read_when_there_is_one() {
        let body = r#"{"icestats":{"source":{"mount":"/s","listeners":"3","listener_peak":"9"}}}"#;
        let counts = parse_counts(body, Some("/s")).unwrap();
        assert_eq!((counts.listeners, counts.peak), (3, 9));
    }

    #[test]
    fn a_page_that_is_not_a_status_document_is_an_error_not_a_zero() {
        assert!(parse_counts("<html>404</html>", None).is_err());
        assert!(parse_counts(r#"{"hello":"world"}"#, None).is_err());
    }

    /// Hits a real server, the one this was built against: an RSAS install
    /// whose source mount is `live.mp3` and whose Liquidsoap relay republishes
    /// it as `stream.mp3`. Ignored by default like the other live tests in this
    /// crate; run with `cargo test --include-ignored real_icecast_status`.
    ///
    /// Both halves are exercised, because it is the *difference* between them
    /// that the feature exists for: the relay mount is found and counted, and
    /// the mount the source publishes to is reported as missing rather than as
    /// an idle mount whenever nothing is broadcasting to it.
    #[test]
    #[ignore]
    fn real_icecast_status() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let fetch = |url: String| {
            runtime.block_on(async move { reqwest::get(&url).await.unwrap().text().await.unwrap() })
        };

        let relay = stats_target("radio.oriolgomez.com", 8000, "live.mp3", "stream.mp3").unwrap();
        assert_eq!(
            relay.status_url,
            "http://radio.oriolgomez.com:8000/status-json.xsl"
        );
        let body = fetch(relay.status_url.clone());
        let counts = parse_counts(&body, relay.mount.as_deref()).unwrap();
        assert!(counts.matched, "/stream.mp3 was not in {body}");
        println!("{} listeners on /stream.mp3", counts.listeners);

        // The same server read the old way: the source mount, which is where
        // the count used to have to come from and where it is not.
        let source = stats_target("radio.oriolgomez.com", 8000, "live.mp3", "").unwrap();
        assert_eq!(source.mount.as_deref(), Some("/live.mp3"));
        let counts = parse_counts(&body, source.mount.as_deref()).unwrap();
        println!(
            "/live.mp3 present: {}, listeners {}",
            counts.matched, counts.listeners
        );

        // And the whole server, which is the form a listener URL naming no
        // mount resolves to.
        let all = stats_target(
            "radio.oriolgomez.com",
            8000,
            "live.mp3",
            "http://radio.oriolgomez.com:8000/",
        )
        .unwrap();
        assert_eq!(all.mount, None);
        let counts = parse_counts(&body, None).unwrap();
        assert!(counts.matched);
        println!("{} listeners across every mount", counts.listeners);
    }
}
