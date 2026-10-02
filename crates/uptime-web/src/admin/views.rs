//! Admin console components.

use jiff::Timestamp;
use topcoat::{
    Result,
    view::{View, component, view},
};
use uptime_domain::{CheckSpec, MonitorState};
use uptime_store::MonitorOverview;

use super::form::{FormErrors, MonitorForm};
use crate::views::{ago, state_pill};

/// Where a check points, for tables: the URL or `host:port`.
pub(crate) fn target(check: &CheckSpec) -> String {
    match check {
        CheckSpec::Http(http) => http.url.to_string(),
        CheckSpec::Tcp(tcp) => format!("{}:{}", tcp.host, tcp.port),
        CheckSpec::Dns(dns) => format!("{} {}", dns.record_type, dns.name),
        CheckSpec::Push(_) => "push heartbeat".to_owned(),
    }
}

/// Counts per state, shown above the table.
#[component]
pub(crate) async fn summary(overviews: &[MonitorOverview]) -> Result<impl View> {
    let count = |states: &[MonitorState]| {
        overviews
            .iter()
            .filter(|o| states.contains(&o.runtime.runtime.state))
            .count()
    };
    let chips = [
        (
            MonitorState::Up,
            count(&[MonitorState::Up, MonitorState::Degraded]),
        ),
        (MonitorState::Down, count(&[MonitorState::Down])),
        (MonitorState::Pending, count(&[MonitorState::Pending])),
        (MonitorState::Paused, count(&[MonitorState::Paused])),
    ];
    Ok(view! {
        <div id="summary" class="summary">
            for (state, n) in chips {
                <span class=(format!("pill pill-{}", state.as_str()))>
                    (n) " " (crate::views::state_label(state).to_lowercase())
                </span>
            }
        </div>
    })
}

/// A row shows when it matches the search text and the tag filter.
const FILTER_SHOW: &str = "(!$q || el.dataset.q.includes($q.toLowerCase())) && (!$tagf || el.dataset.tags.split(' ').includes($tagf))";

/// One dashboard row; its id lets the live stream morph it in place.
#[component]
pub(crate) async fn monitor_row(
    overview: &MonitorOverview,
    now: Timestamp,
    #[default] depth: usize,
) -> Result<impl View> {
    let monitor = &overview.monitor;
    let runtime = &overview.runtime;
    let href = format!("/admin/monitors/{}", monitor.id);
    let latency = runtime
        .last_latency_ms
        .map(|ms| format!("{ms}ms"))
        .unwrap_or_else(|| "—".into());
    let paused = runtime.runtime.state == MonitorState::Paused;
    let search_text = format!(
        "{} {} {} {}",
        monitor.spec.name,
        monitor.spec.key,
        target(&monitor.spec.check),
        monitor.spec.tags.join(" ")
    )
    .to_lowercase();
    Ok(view! {
        <tr id=(format!("monitor-{}", monitor.id))
            data-id=(monitor.id.to_string())
            data-q=(search_text)
            data-tags=(monitor.spec.tags.join(" "))
            style=(format!("--depth: {depth}"))
            data-show=(FILTER_SHOW)>
            <td class="select">
                <input type="checkbox" value=(monitor.id.to_string())
                    aria-label=(format!("Select {}", monitor.spec.name))
                    data-effect=(format!("el.checked = $sel.includes('{}')", monitor.id))
                    data-on:change=(format!("$sel = evt.target.checked ? [...$sel, '{id}'] : $sel.filter(x => x !== '{id}')", id = monitor.id))>
            </td>
            <td title=(runtime.last_error.clone())>state_pill(state: runtime.runtime.state)</td>
            <td class="name-cell">
                <a href=(href.clone())>(monitor.spec.name.as_str())</a>
                <div class="muted small mono">(monitor.spec.key.as_str())</div>
                if !monitor.spec.tags.is_empty() {
                    <ul class="chips small">
                        for tag in &monitor.spec.tags {
                            <li>(tag.as_str())</li>
                        }
                    </ul>
                }
            </td>
            <td class="mono small hide-sm">(target(&monitor.spec.check))</td>
            <td class="num">(latency)</td>
            <td class="small when hide-sm">
                if let Some(at) = runtime.last_checked_at { ago(at: at, now: now) } else { "never" }
            </td>
            <td class="small muted when hide-sm">
                if paused { "paused" } else { ago(at: runtime.next_run_at, now: now) }
            </td>
            <td class="row-actions">
                <a href=(format!("{href}/edit"))>"Edit"</a>
                <a href=(format!("/admin/monitors/new?from={}", monitor.id))>"Duplicate"</a>
            </td>
        </tr>
    })
}

/// A labelled input with its error message.
#[component]
pub(crate) async fn text_field(
    name: &str,
    label: &str,
    value: &str,
    errors: &FormErrors,
    #[default] hint: &str,
    #[default] placeholder: &str,
) -> Result<impl View> {
    let error = errors.get(name).map(str::to_owned);
    let id = format!("field-{name}");
    Ok(view! {
        <div class="field">
            <label for=(id.clone())>(label)</label>
            <input
                type="text"
                id=(id)
                name=(name)
                value=(value)
                placeholder=((!placeholder.is_empty()).then_some(placeholder))
                aria-invalid=(error.is_some().then_some("true"))
            >
            if let Some(error) = error {
                <span class="error">(error)</span>
            } else if !hint.is_empty() {
                <span class="hint">(hint)</span>
            }
        </div>
    })
}

/// A labelled multi-line input with its error message.
#[component]
pub(crate) async fn text_area(
    name: &str,
    label: &str,
    value: &str,
    errors: &FormErrors,
    #[default] hint: &str,
) -> Result<impl View> {
    let error = errors.get(name).map(str::to_owned);
    let id = format!("field-{name}");
    Ok(view! {
        <div class="field">
            <label for=(id.clone())>(label)</label>
            <textarea id=(id) name=(name) rows=((value.lines().count() + 2).clamp(3, 24).to_string())
                spellcheck="false" aria-invalid=(error.is_some().then_some("true"))>(value)</textarea>
            if let Some(error) = error {
                <span class="error">(error)</span>
            } else if !hint.is_empty() {
                <span class="hint">(hint)</span>
            }
        </div>
    })
}

#[component]
pub(crate) async fn checkbox(name: &str, label: &str, checked: bool) -> Result<impl View> {
    Ok(view! {
        <div class="check">
            <sb-toggle name=(name) label=(label) checked=(if checked { "true" } else { "false" })></sb-toggle>
        </div>
    })
}

/// The create/edit form. `action` is where it posts.
#[component]
pub(crate) async fn monitor_form(
    form: &MonitorForm,
    errors: &FormErrors,
    action: &str,
    cancel: &str,
    #[default] channels: &[uptime_store::Channel],
) -> Result<impl View> {
    let check_type = if form.check_type == "tcp" {
        "tcp"
    } else {
        "http"
    };
    let methods = ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"];
    let channel_toggles: Vec<(String, String, bool)> = channels
        .iter()
        .map(|channel| {
            (
                format!("channel_{}", channel.id),
                format!("{} · {}", channel.spec.name, channel.spec.kind.label()),
                form.has_channel(channel.id),
            )
        })
        .collect();
    let method = if form.method.is_empty() {
        "GET".to_owned()
    } else {
        form.method.to_ascii_uppercase()
    };
    Ok(view! {
        <form method="post" action=(action) data-signals:check_type=(format!("'{check_type}'")) data-signals:auth_type=(format!("'{}'", form.auth_type))>
            if !errors.is_empty() {
                <p class="flash flash-error" role="alert">"Some fields need attention."</p>
            }
            <fieldset>
                <legend>"Monitor"</legend>
                <div class="fields">
                    text_field(name: "name", label: "Name", value: &form.name, errors: errors, placeholder: "Public API")
                    text_field(name: "key", label: "Key", value: &form.key, errors: errors,
                        hint: "Stable id used by status pages: lowercase, digits, dashes.", placeholder: "public-api")
                </div>
                text_field(name: "group", label: "Group", value: &form.group, errors: errors,
                    placeholder: "prod/eu", hint: "Optional. Slash-separated path: the dashboard shows monitors as a tree, each node with its worst state.")
                text_field(name: "tags", label: "Tags", value: &form.tags, errors: errors,
                    placeholder: "prod, api", hint: "Optional, comma-separated. Filter and bulk-edit the dashboard by tag.")
                checkbox(name: "active", label: "Active (checks run on schedule)", checked: form.active.is_some())
            </fieldset>

            <fieldset>
                <legend>"Check"</legend>
                <div class="field">
                    <label for="field-check_type">"Type"</label>
                    <select id="field-check_type" name="check_type" data-bind:check_type="">
                        <option value="http" selected=(check_type == "http")>"HTTP(S)"</option>
                        <option value="tcp" selected=(check_type == "tcp")>"TCP port"</option>
                        <option value="dns" selected=(check_type == "dns")>"DNS lookup"</option>
                        <option value="push" selected=(check_type == "push")>"Push (heartbeat from the service)"</option>
                    </select>
                </div>
                <div data-show="$check_type == 'dns'">
                    <div class="fields">
                        text_field(name: "dns_name", label: "Name", value: &form.dns_name, errors: errors, placeholder: "example.com")
                        <div class="field">
                            <label for="field-record_type">"Record type"</label>
                            <select id="field-record_type" name="record_type">
                                for kind in uptime_domain::DnsRecordType::ALL {
                                    <option value=(kind.as_str()) selected=(form.record_type.eq_ignore_ascii_case(kind.as_str()))>(kind.as_str())</option>
                                }
                            </select>
                        </div>
                    </div>
                    <div class="fields">
                        text_field(name: "resolver", label: "Resolver", value: &form.resolver, errors: errors,
                            placeholder: "1.1.1.1", hint: "Optional; blank uses the system resolver.")
                        text_field(name: "expect", label: "Expected answer", value: &form.expect, errors: errors,
                            hint: "Optional text one answer must contain, e.g. an IP or v=spf1.")
                    </div>
                </div>
                <div data-show="$check_type == 'push'">
                    <input type="hidden" name="push_token" value=(form.push_token.as_str())>
                    <p class="small muted">"The service calls its push URL at least once per interval: "
                        <code>"GET /api/push/<token>?status=up&msg=…&ping=ms"</code>
                        ". Silence, or " <code>"status=down"</code> ", counts as a failure. The URL is shown after saving."</p>
                </div>
                <div data-show="$check_type == 'http'">
                    text_field(name: "url", label: "URL", value: &form.url, errors: errors, placeholder: "https://api.example.com/health")
                    <div class="fields">
                        <div class="field">
                            <label for="field-method">"Method"</label>
                            <select id="field-method" name="method">
                                for m in methods {
                                    <option value=(m) selected=(m == method)>(m)</option>
                                }
                            </select>
                        </div>
                        text_field(name: "accepted_status", label: "Accepted status codes", value: &form.accepted_status,
                            errors: errors, hint: "e.g. 200-299, or 403 for \"must be forbidden\".")
                        text_field(name: "max_redirects", label: "Max redirects", value: &form.max_redirects,
                            errors: errors, hint: "0 checks the first response itself.")
                    </div>
                    <div class="fields">
                        text_field(name: "keyword", label: "Keyword", value: &form.keyword, errors: errors,
                            hint: "Optional text the response body must contain.")
                        <div class="field">
                            <label>"Keyword rule"</label>
                            checkbox(name: "keyword_absent", label: "Fail if the keyword IS present", checked: form.keyword_absent.is_some())
                        </div>
                    </div>
                    <div class="fields">
                        text_field(name: "json_path", label: "JSON path", value: &form.json_path, errors: errors,
                            placeholder: "data.status", hint: "Optional: the response must be JSON with a value here.")
                        text_field(name: "json_expect", label: "Expected JSON value", value: &form.json_expect, errors: errors,
                            hint: "Blank means the path only has to exist.")
                    </div>
                    <div class="field">
                        <label for="field-auth_type">"Authentication"</label>
                        <select id="field-auth_type" name="auth_type" data-bind:auth_type="">
                            <option value="" selected=(form.auth_type.is_empty())>"None"</option>
                            <option value="basic" selected=(form.auth_type == "basic")>"Basic (username and password)"</option>
                            <option value="bearer" selected=(form.auth_type == "bearer")>"Bearer token"</option>
                        </select>
                    </div>
                    <div class="fields" data-show="$auth_type == 'basic'">
                        text_field(name: "auth_username", label: "Username", value: &form.auth_username, errors: errors)
                        text_field(name: "auth_password", label: "Password", value: &form.auth_password, errors: errors)
                    </div>
                    <div class="fields" data-show="$auth_type == 'bearer'">
                        text_field(name: "auth_token", label: "Token", value: &form.auth_token, errors: errors)
                    </div>
                    text_area(name: "headers", label: "Request headers", value: &form.headers, errors: errors,
                        hint: "One per line: Name: value")
                    text_area(name: "body", label: "Request body", value: &form.body, errors: errors)
                    <div class="fields">
                        text_field(name: "cert_warn_days", label: "Certificate warning", value: &form.cert_warn_days, errors: errors,
                            hint: "Alert this many days before the HTTPS certificate expires (0 = never).")
                    </div>
                    checkbox(name: "ignore_tls_errors", label: "Ignore TLS certificate errors", checked: form.ignore_tls_errors.is_some())
                </div>
                <div class="fields" data-show="$check_type == 'tcp'">
                    text_field(name: "host", label: "Host", value: &form.host, errors: errors, placeholder: "db.example.com")
                    text_field(name: "port", label: "Port", value: &form.port, errors: errors, placeholder: "5432")
                </div>
                <div data-show="$check_type == 'tcp'">
                    <div class="fields">
                        text_field(name: "tcp_send", label: "Send", value: &form.tcp_send, errors: errors,
                            placeholder: "PING\\r\\n", hint: "Optional text sent once connected (\\r \\n \\t escapes).")
                        text_field(name: "tcp_expect", label: "Expect", value: &form.tcp_expect, errors: errors,
                            placeholder: "+PONG", hint: "Optional: the reply must contain this. Blank send waits for a banner (220, SSH-, ...).")
                    </div>
                    <div class="fields">
                        text_field(name: "tcp_cert_warn_days", label: "Certificate warning", value: &form.tcp_cert_warn_days,
                            errors: errors, hint: "TLS only: alert this many days before the certificate expires (0 = never).")
                    </div>
                    checkbox(name: "tcp_tls", label: "Connect with TLS (implicit TLS: SMTPS, IMAPS, LDAPS, ...)", checked: form.tcp_tls.is_some())
                    checkbox(name: "tcp_ignore_tls_errors", label: "Ignore TLS certificate errors", checked: form.tcp_ignore_tls_errors.is_some())
                </div>
            </fieldset>

            <fieldset>
                <legend>"Schedule & rules"</legend>
                <div class="fields">
                    text_field(name: "interval", label: "Interval", value: &form.interval, errors: errors, hint: "How often to check, e.g. 30s or 1m.")
                    text_field(name: "timeout", label: "Timeout", value: &form.timeout, errors: errors, hint: "e.g. 10s; at most 80% of the interval.")
                    text_field(name: "retries", label: "Retries", value: &form.retries, errors: errors, hint: "Failures tolerated before DOWN.")
                    text_field(name: "retry_interval", label: "Retry interval", value: &form.retry_interval, errors: errors, hint: "Cadence while a failure is unconfirmed.")
                    text_field(name: "degraded_after", label: "Degraded after", value: &form.degraded_after, errors: errors,
                        placeholder: "800ms",
                        hint: "Optional. Responses slower than this count as DEGRADED, e.g. 800ms, 1.5s or 2s; keep it below the timeout. Blank: never degraded.")
                    text_field(name: "resend_every", label: "Re-notify every", value: &form.resend_every, errors: errors, hint: "While DOWN, remind every N checks (0 = never).")
                </div>
                checkbox(name: "invert", label: "Upside-down: UP means the target is NOT reachable", checked: form.invert.is_some())
            </fieldset>

            <fieldset>
                <legend>"Alerts"</legend>
                if channels.is_empty() {
                    <p class="muted small">"No alert channels yet. " <a href="/admin/alerts/new">"Add Slack, Discord or a webhook"</a> " to be told when this monitor goes down."</p>
                } else {
                    <p class="muted small">"Where to send DOWN and RECOVERED alerts for this monitor."</p>
                    for (name, label, checked) in &channel_toggles {
                        checkbox(name: name, label: label, checked: *checked)
                    }
                }
            </fieldset>

            <div class="actions">
                <button class="btn btn-primary" type="submit">"Save monitor"</button>
                <button class="btn" type="button"
                    data-on:click="@post('/admin/monitors/test', {contentType: 'form'})"
                    data-indicator:_testing=""
                    data-attr:aria-busy="$_testing">"Test now"</button>
                <a class="btn btn-link" href=(cancel)>"Cancel"</a>
            </div>
            <div id="test-result" class="stack"></div>
        </form>
    })
}
