//! The hosted checkout page — server-rendered, self-contained HTML.
//!
//! One screen: show the amount, let the customer pick bKash/Nagad, display the
//! merchant's receiving number with copy + "send exactly ৳X" instructions, then
//! poll until the SMS forwarder settles the charge and redirect to `return_url`.
//! A collapsible manual box lets them submit a TrxID if the SMS is delayed.
//! Light/clean hosted-checkout styling. Author: Mohammad Sijan (SpritexAI).
//!
//! ponytail: inline HTML/CSS/JS in one Rust string — a checkout is a single screen;
//! a templating engine + asset pipeline would be overkill. Extract only if it grows.

use crate::checkout::PublicCharge;

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

pub fn not_found() -> String {
    wrap(
        "Payment not found",
        r#"<div class="card"><h1>Payment link invalid</h1>
        <p class="muted">This checkout link is unknown or has expired. Please return to the store and try again.</p></div>"#,
    )
}

pub fn render(view: &PublicCharge) -> String {
    let amount = format!("{:.2}", view.amount_minor as f64 / 100.0);
    let symbol = if view.currency == "BDT" { "৳" } else { "" };

    // Build the gateway options from the merchant's registered receivers.
    let mut options = String::new();
    for r in &view.receivers {
        let g = esc(&r.gateway);
        let name = match r.gateway.as_str() {
            "bkash" => "bKash",
            "nagad" => "Nagad",
            other => other,
        };
        let number = r.account_msisdn.clone().unwrap_or_default();
        options.push_str(&format!(
            r#"<button class="gw" data-gateway="{g}" data-number="{num}">
                 <span class="gw-name">{name}</span>
                 <span class="gw-num">{num}</span>
               </button>"#,
            g = g,
            name = name,
            num = esc(&number),
        ));
    }
    if options.is_empty() {
        options.push_str(
            r#"<p class="muted">The merchant has not configured a receiving number yet.</p>"#,
        );
    }

    let body = format!(
        r#"<div class="card">
      <div class="amount"><span class="sym">{symbol}</span>{amount}</div>
      <p class="muted">Complete your payment to continue.</p>

      <div id="pick">
        <div class="label">Pay with</div>
        <div class="gateways">{options}</div>
      </div>

      <div id="instructions" style="display:none">
        <div class="steps">
          <div class="step"><span>1</span> Open your <b id="gw-label"></b> app and choose <b>Send Money</b>.</div>
          <div class="step"><span>2</span> Send exactly <b>{symbol}{amount}</b> to this number:</div>
          <div class="number-box">
            <span id="recv-number" class="number"></span>
            <button id="copy" class="copy">Copy</button>
          </div>
          <div class="step"><span>3</span> Keep this page open — we confirm your payment automatically.</div>
        </div>

        <div class="waiting">
          <span class="spinner"></span>
          <span id="status-text">Waiting for your payment…</span>
        </div>

        <details class="manual">
          <summary>Already paid? Enter your Transaction ID</summary>
          <input id="trx" placeholder="TrxID (e.g. BGL7AH92KX)" />
          <input id="sender" placeholder="Your number (optional)" />
          <button id="submit-trx" class="submit">Submit</button>
          <p id="manual-note" class="muted small"></p>
        </details>
      </div>

      <div id="done" style="display:none">
        <div class="success">✓</div>
        <h1>Payment confirmed</h1>
        <p class="muted">Redirecting you back…</p>
      </div>
    </div>

    <script>
      const payRef = {pay_ref:?};
      const returnUrl = {return_url:?};
      let selected = null;

      document.querySelectorAll('.gw').forEach(btn => {{
        btn.addEventListener('click', () => {{
          selected = btn.dataset.gateway;
          document.getElementById('gw-label').textContent = btn.querySelector('.gw-name').textContent;
          document.getElementById('recv-number').textContent = btn.dataset.number || '—';
          document.getElementById('pick').style.display = 'none';
          document.getElementById('instructions').style.display = 'block';
          fetch('/v1/checkout/' + payRef + '/select', {{
            method: 'POST', headers: {{'content-type':'application/json'}},
            body: JSON.stringify({{gateway: selected}})
          }}).catch(()=>{{}});
        }});
      }});

      const copyBtn = document.getElementById('copy');
      if (copyBtn) copyBtn.addEventListener('click', () => {{
        const n = document.getElementById('recv-number').textContent;
        navigator.clipboard.writeText(n).then(() => {{
          copyBtn.textContent = 'Copied';
          setTimeout(() => copyBtn.textContent = 'Copy', 1500);
        }});
      }});

      const submitTrx = document.getElementById('submit-trx');
      if (submitTrx) submitTrx.addEventListener('click', () => {{
        const trx = document.getElementById('trx').value.trim();
        if (!trx) return;
        fetch('/v1/checkout/' + payRef + '/claim', {{
          method: 'POST', headers: {{'content-type':'application/json'}},
          body: JSON.stringify({{trx_id: trx, sender: document.getElementById('sender').value.trim() || null}})
        }}).then(() => {{
          document.getElementById('manual-note').textContent =
            'Thanks — we recorded it. Confirmation still completes automatically once your SMS arrives.';
        }}).catch(()=>{{}});
      }});

      function finish() {{
        document.getElementById('instructions').style.display = 'none';
        document.getElementById('pick').style.display = 'none';
        document.getElementById('done').style.display = 'block';
        if (returnUrl && returnUrl !== '--') setTimeout(() => location.href = returnUrl, 1800);
      }}

      // Poll until the SMS-driven settlement flips this charge to paid.
      async function poll() {{
        try {{
          const r = await fetch('/v1/checkout/' + payRef);
          if (r.ok) {{
            const d = await r.json();
            if (d.status === 'paid') {{ finish(); return; }}
          }}
        }} catch (e) {{}}
        setTimeout(poll, 3000);
      }}
      poll();
    </script>"#,
        symbol = symbol,
        amount = amount,
        options = options,
        pay_ref = view.pay_ref,
        return_url = view.return_url.clone().unwrap_or_default(),
    );

    wrap("Checkout", &body)
}

/// Shared page chrome: light, clean, mobile-first. No external assets.
fn wrap(title: &str, inner: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en"><head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>{title} — SpritexAI Pay</title>
<style>
  :root {{ --accent:#6366f1; --ink:#111827; --muted:#6b7280; --line:#e5e7eb; --bg:#f4f5f7; }}
  * {{ box-sizing:border-box; }}
  body {{ margin:0; background:var(--bg); color:var(--ink);
    font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif;
    display:flex; min-height:100vh; align-items:center; justify-content:center; padding:16px; }}
  .card {{ background:#fff; width:100%; max-width:420px; border-radius:16px; padding:28px;
    box-shadow:0 1px 3px rgba(0,0,0,.06), 0 8px 24px rgba(0,0,0,.05); }}
  h1 {{ font-size:20px; margin:0 0 6px; }}
  .muted {{ color:var(--muted); font-size:14px; line-height:1.5; }}
  .small {{ font-size:12px; }}
  .amount {{ font-size:40px; font-weight:700; letter-spacing:-.02em; }}
  .amount .sym {{ color:var(--muted); font-weight:600; margin-right:2px; }}
  .label {{ font-size:12px; text-transform:uppercase; letter-spacing:.04em; color:var(--muted);
    margin:22px 0 10px; }}
  .gateways {{ display:flex; flex-direction:column; gap:10px; }}
  .gw {{ display:flex; justify-content:space-between; align-items:center; width:100%;
    background:#fff; border:1.5px solid var(--line); border-radius:12px; padding:14px 16px;
    cursor:pointer; font-size:15px; transition:border-color .15s, background .15s; }}
  .gw:hover {{ border-color:var(--accent); background:#fafaff; }}
  .gw-name {{ font-weight:600; }}
  .gw-num {{ color:var(--muted); font-family:ui-monospace,monospace; font-size:13px; }}
  .steps {{ margin-top:18px; display:flex; flex-direction:column; gap:14px; }}
  .step {{ display:flex; gap:10px; align-items:flex-start; font-size:14px; line-height:1.5; }}
  .step span {{ flex:0 0 22px; height:22px; border-radius:50%; background:#eef2ff; color:var(--accent);
    font-size:12px; font-weight:700; display:inline-flex; align-items:center; justify-content:center; }}
  .number-box {{ display:flex; align-items:center; gap:8px; background:#f9fafb; border:1px solid var(--line);
    border-radius:10px; padding:12px 14px; }}
  .number {{ font-family:ui-monospace,monospace; font-size:18px; font-weight:600; flex:1; }}
  .copy, .submit {{ background:var(--accent); color:#fff; border:0; border-radius:8px;
    padding:8px 14px; font-size:13px; font-weight:600; cursor:pointer; }}
  .waiting {{ display:flex; align-items:center; gap:10px; margin-top:22px; font-size:14px; color:var(--muted); }}
  .spinner {{ width:16px; height:16px; border:2px solid var(--line); border-top-color:var(--accent);
    border-radius:50%; animation:spin 1s linear infinite; }}
  @keyframes spin {{ to {{ transform:rotate(360deg); }} }}
  .manual {{ margin-top:20px; border-top:1px solid var(--line); padding-top:16px; }}
  .manual summary {{ cursor:pointer; font-size:14px; color:var(--accent); }}
  .manual input {{ display:block; width:100%; margin-top:10px; padding:11px 13px;
    border:1px solid var(--line); border-radius:9px; font-size:14px; }}
  .manual .submit {{ margin-top:10px; }}
  .success {{ font-size:44px; color:#16a34a; text-align:center; }}
  #done {{ text-align:center; }}
  .brand {{ text-align:center; margin-top:18px; color:var(--muted); font-size:12px; }}
</style></head>
<body>
  <div>{inner}
    <div class="brand">Secured by <b>SpritexAI Pay</b></div>
  </div>
</body></html>"#,
        title = esc(title),
        inner = inner,
    )
}
