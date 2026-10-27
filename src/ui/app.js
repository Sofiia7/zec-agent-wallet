'use strict';

// Owner window. All text that came from outside (memos, labels, messages, names)
// is inserted with textContent only.

let csrf = null;
let cache = {};
const ui = {
  screen: 'overview',
  filter: 'all',
  open: new Set(),      // bills whose "what the agent sees" panel is open
  approving: null,      // bill whose approval form is open
  limitDraft: null,
  limitSaved: false,
  copied: false,
  topup: null,          // null, 'loading' or { address, qr }
};

function h(tag, attrs, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === null || v === undefined || v === false) continue;
    if (k === 'class') el.className = v;
    else if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
    else el.setAttribute(k, v === true ? '' : v);
  }
  for (const c of children.flat(Infinity)) {
    if (c === null || c === undefined || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

async function api(method, path, body) {
  const opts = { method, headers: {} };
  if (body !== undefined) {
    opts.headers['content-type'] = 'application/json';
    opts.body = JSON.stringify(body);
  }
  if (method !== 'GET' && csrf) opts.headers['x-csrf-token'] = csrf;
  let res;
  try { res = await fetch('/api/owner' + path, opts); } catch (_) {
    throw new Error('the wallet is not answering; check that zaw daemon is still running');
  }
  let data = null;
  try { data = await res.json(); } catch (_) { data = null; }
  // A wrong password names its field; only a 401 without one means the session ended.
  if (res.status === 401 && !(data && data.field)) {
    csrf = null;
    renderLogin();
    const e = new Error('login required');
    e.login = true;
    throw e;
  }
  if (!res.ok) {
    const e = new Error((data && data.error) || ('request failed: ' + res.status));
    e.field = data && data.field;
    throw e;
  }
  return data;
}

function zec(zat) {
  if (zat === null || zat === undefined) return '-';
  const whole = Math.floor(zat / 1e8);
  const frac = String(zat % 1e8).padStart(8, '0').replace(/0+$/, '');
  return frac ? `${whole}.${frac}` : String(whole);
}

function day(unix) {
  return new Date(unix * 1000).toLocaleDateString('en-GB', { day: '2-digit', month: 'short' }).toUpperCase();
}

function clock(unix) {
  return new Date(unix * 1000).toLocaleTimeString('en-GB', { hour: '2-digit', minute: '2-digit' });
}

function stamp(unix) {
  return `${new Date(unix * 1000).toLocaleDateString('en-GB', { day: '2-digit', month: 'short' })}, ${clock(unix)}`;
}

function coin() {
  return cache.overview && cache.overview.network === 'mainnet' ? 'ZEC' : 'TAZ';
}

// Approximate dollars, only when the owner turned USD amounts on (mainnet only).
function usd(zat) {
  const q = cache.overview && cache.overview.usd;
  if (!q || zat === null || zat === undefined) return '';
  const micro = (zat * q.micro_usd_per_zec) / 1e8;
  const cents = Math.round(micro / 1e4);
  if (cents === 0 && micro > 0) return '≈ <$0.01';
  return '≈ $' + (cents / 100).toLocaleString('en-US', { minimumFractionDigits: 2, maximumFractionDigits: 2 });
}

function withUsd(text, zat) {
  const u = usd(zat);
  return u ? `${text} (${u})` : text;
}

function errorLine() { return h('div', { class: 'err', role: 'alert' }); }

// Daemon messages are lower-case fragments; show them as sentences.
function sentence(s) {
  s = String(s || '').trim();
  if (!s) return s;
  s = s[0].toUpperCase() + s.slice(1);
  return /[.!?]$/.test(s) ? s : s + '.';
}

// Amounts as the daemon reads them; a comma works as the decimal point too.
function amount(v) { return v.trim().replace(',', '.'); }

function amountProblem(v, example, zeroOk) {
  const s = amount(v);
  if (!/^(\d+(\.\d{0,8})?|\.\d{1,8})$/.test(s)) return `Type an amount like ${example}: digits and one point, at most 8 digits after it`;
  if (Number(s) === 0 && !zeroOk) return 'The amount must be more than 0';
  if (Number(s) > 21e6) return 'That is more than all the ZEC there is';
  return '';
}

// ---- forms and messages --------------------------------------------------------
// Every field says whether it is required and shows its own error right under it.

let fieldSeq = 0;

function field(name, text, input, opts = {}) {
  const id = `f${++fieldSeq}`;
  input.id = id;
  input.setAttribute('aria-describedby', `${id}-err`);
  const f = { name, input, optional: !!opts.optional, empty: opts.empty, check: opts.check };
  f.err = h('div', { class: 'ferr', id: `${id}-err` });
  f.el = h('div', { class: 'f' + (opts.wide ? ' wide' : '') },
    h('label', { class: 'fl', for: id }, text,
      h('span', { class: opts.optional ? 'mark' : 'mark must' }, opts.optional ? 'optional' : 'required')),
    opts.after ? h('div', { class: 'frow' }, input, opts.after) : input,
    opts.hint ? h('div', { class: 'hint' }, opts.hint) : null,
    f.err);
  input.addEventListener('input', () => setFieldError(f, ''));
  return f;
}

function setFieldError(f, message) {
  f.err.textContent = message ? sentence(message) : '';
  f.el.classList.toggle('invalid', !!message);
  if (message) f.input.setAttribute('aria-invalid', 'true');
  else f.input.removeAttribute('aria-invalid');
}

// Checks the fields, then runs the request. The button says what is happening while
// it runs. An error goes under the field the daemon names, otherwise into the box.
async function submit(fields, box, button, busy, fn) {
  box.textContent = '';
  const summary = n => (fields.length > 1 ? (n === 1 ? 'Check the field marked in red.' : `Check the ${n} fields marked in red.`) : '');
  const bad = [];
  for (const f of fields) {
    const v = f.input.value.trim();
    const problem = !v ? (f.optional ? '' : f.empty || 'Fill in this field') : (f.check && f.check(v)) || '';
    setFieldError(f, problem);
    if (problem) bad.push(f);
  }
  if (bad.length) {
    box.textContent = summary(bad.length);
    bad[0].input.focus();
    return;
  }
  const text = button.textContent;
  button.disabled = true;
  button.textContent = busy;
  try {
    await fn();
  } catch (e) {
    const f = fields.find(x => x.name === e.field);
    if (f) {
      setFieldError(f, e.message);
      box.textContent = summary(1);
      f.input.focus();
    } else if (!e.login) {
      box.textContent = sentence(e.message);
    }
  } finally {
    button.disabled = false;
    button.textContent = text;
  }
}

// A short message at the top of the window: what just happened, or why it did not.
function flash(kind, text) {
  for (const old of document.querySelectorAll('.flash')) old.remove();
  const el = h('div', { class: `flash ${kind}`, role: kind === 'bad' ? 'alert' : 'status' }, h('span', {}, text));
  el.append(h('button', { class: 'x', type: 'button', 'aria-label': 'Close', onclick: () => el.remove() }, '×'));
  document.body.append(el);
  setTimeout(() => el.remove(), kind === 'bad' ? 15000 : 5000);
}

// Buttons outside forms: a result or a failure is always shown.
function act(fn, done) {
  return async (ev) => {
    const button = ev.currentTarget;
    button.disabled = true;
    try {
      await fn();
      if (done) flash('ok', done);
    } catch (e) {
      if (!e.login) flash('bad', sentence(e.message));
    } finally {
      button.disabled = false;
    }
  };
}

// ---- theme -----------------------------------------------------------------

function storedTheme() {
  try { return localStorage.getItem('zaw-theme'); } catch (_) { return null; }
}

function applyTheme(theme) {
  document.documentElement.dataset.theme = theme;
  try { localStorage.setItem('zaw-theme', theme); } catch (_) { /* private window */ }
}

document.documentElement.dataset.theme = storedTheme()
  || (window.matchMedia && matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light');

// ---- bills: names and states ------------------------------------------------

function billName(b) {
  return b.bill.label || b.bill.ext_label || `Bill ${b.bill.id.slice(5, 11)}`;
}

function payeeText(b) {
  if (b.contact && b.address_matches_contact) return `${b.contact.local_name} · saved address`;
  if (b.contact) return `${b.contact.local_name} · address not saved`;
  return 'Unknown recipient';
}

function state(b) {
  const s = b.bill.status;
  if (s === 'needs_approval') return 'needs';
  return s;
}

function statusLabel(b) {
  const s = state(b);
  if (s === 'needs') return h('span', { class: 'stat needs' }, h('span', { class: 'dot' }), b.approval_requested ? 'Agent asks' : 'Needs you');
  const text = { approved: 'Approved', paying: 'Paying', paid: 'Paid', rejected: 'Rejected', failed: 'Failed', expired_unpaid: 'Expired' }[s] || s;
  const cls = s === 'approved' || s === 'paying' ? 'stat' : (s === 'failed' || s === 'expired_unpaid') ? 'stat bad' : 'stat quiet';
  return h('span', { class: cls }, text);
}

function whenText(b) {
  const s = state(b);
  const op = b.operation;
  if (s === 'needs') return `waiting since ${stamp(b.bill.created_at)}`;
  if (s === 'approved' && b.approval) return `OK until ${stamp(b.approval.expires_at)}`;
  if (s === 'paying' && op) return op.state.replace(/_/g, ' ');
  if (s === 'paid' && op) return `paid ${stamp(op.updated_at)}`;
  if (s === 'failed') return 'nothing was sent';
  if (s === 'expired_unpaid') return 'expired unpaid';
  return '';
}

function overLimit(b) {
  const o = cache.overview;
  const s = state(b);
  return (s === 'needs' || s === 'approved') && o && b.bill.amount > o.daily_limit;
}

function goApprove(b) {
  ui.screen = 'bills';
  ui.filter = 'all';
  ui.approving = b.bill.id;
  render();
}

// ---- activity in plain words -----------------------------------------------

function describe(e, index) {
  const c = coin();
  const bill = index.bills[e.ref_id] || index.byOp[e.ref_id] || index.byApproval[e.ref_id];
  const req = index.requests[e.ref_id];
  const name = bill ? `“${billName(bill)}”` : req ? `“${req.request.label}”` : '';
  const amount = bill ? `${zec(bill.bill.amount)} ${c}` : req ? `${zec(req.request.amount)} ${c}` : '';
  const source = e.detail === 'inbox' ? 'Inbox' : e.detail === 'playground' ? 'Playground' : 'You';
  const t = {
    bill_imported: [`Bill added: ${name}`, source],
    bill_approved: [`You approved ${name}`, 'You'],
    bill_rejected: [`You rejected ${name}`, 'You'],
    approval_revoked: [`You revoked the approval for ${name}`, 'You'],
    approval_requested: [`Agent asked you to approve ${name}`, 'Agent'],
    approval_mismatch: [`Refused to pay ${name}: it no longer matches the approval`, 'Wallet'],
    payment_reserved: [`Agent started paying ${name}`, 'Agent'],
    payment_signed: [`Payment for ${name} signed`, 'Wallet'],
    payment_submitted: [`Payment for ${name} sent to the network`, 'Wallet'],
    payment_confirmed: [`Payment for ${name} confirmed`, 'Wallet'],
    payment_failed: [`Payment for ${name} failed, nothing was sent`, 'Wallet'],
    payment_expired: [`Payment for ${name} expired unpaid`, 'Wallet'],
    request_created: [`Agent asked to be paid ${name}`, 'Agent'],
    owner_login: ['You unlocked the wallet', 'You'],
    owner_locked: ['You locked the wallet', 'You'],
    owner_password_failed: ['Wrong password', ''],
    owner_daily_limit_changed: ['You changed the daily limit', 'You'],
    owner_paused: ['You stopped agent payments', 'You'],
    owner_resumed: ['You resumed agent payments', 'You'],
    owner_usd_on: ['You turned on USD amounts', 'You'],
    owner_usd_off: ['You turned off USD amounts', 'You'],
    inbox_file_rejected: ['A file in the inbox was not a valid bill', 'Wallet'],
    leak_blocked: ['Blocked a response to the agent that looked like an address or key', 'Wallet'],
  }[e.kind] || [e.kind.replace(/_/g, ' '), ''];
  return { at: e.at, text: t[0].trim(), who: t[1], ref: amount };
}

function activity() {
  const index = { bills: {}, byOp: {}, byApproval: {}, requests: {} };
  for (const b of cache.bills || []) {
    index.bills[b.bill.id] = b;
    if (b.operation) index.byOp[b.operation.id] = b;
    if (b.approval) index.byApproval[b.approval.id] = b;
  }
  for (const r of cache.requests || []) index.requests[r.request.id] = r;
  const rows = [];
  for (const e of cache.events || []) {
    const d = describe(e, index);
    const last = rows[rows.length - 1];
    if (last && last.text === d.text && last.who === d.who) { last.n += 1; continue; }
    rows.push({ ...d, n: 1 });
  }
  return rows.map(r => ({ ...r, text: r.n > 1 ? `${r.text} (${r.n} times)` : r.text }));
}

// ---- login -----------------------------------------------------------------

function renderLogin() {
  const pw = field('password', 'Wallet password', h('input', { type: 'password', class: 'u', autocomplete: 'current-password' }),
    { empty: 'Type the wallet password', hint: 'The password you chose when you ran zaw init.' });
  const box = errorLine();
  const button = h('button', { class: 'pill ink', type: 'submit' }, 'Unlock');
  const form = h('form', { novalidate: true },
    h('div', { class: 'eyebrow' }, 'Owner window'),
    h('h1', {}, 'Unlock'),
    h('p', {}, 'The password decrypts the seed. While unlocked, the agent can pay bills you approved; nothing else.'),
    pw.el, box, button);
  form.addEventListener('submit', (ev) => {
    ev.preventDefault();
    submit([pw], box, button, 'Unlocking…', async () => {
      const res = await api('POST', '/login', { password: pw.input.value });
      csrf = res.csrf;
      pw.input.value = '';
      start();
    });
  });
  document.getElementById('root').replaceChildren(
    h('div', { class: 'login' }, h('div', { class: 'brand' }, h('b', {}, 'Zec Agent Wallet')), form));
  pw.input.focus();
}

// ---- sidebar ---------------------------------------------------------------

function renderSide() {
  const o = cache.overview;
  const needs = (cache.bills || []).filter(b => state(b) === 'needs').length;
  const tabs = [['overview', 'Overview'], ['bills', 'Bills'], ['activity', 'Activity'], ['settings', 'Settings']];
  const sync = o.sync;
  const behind = sync ? sync.chain_tip - sync.fully_scanned : null;
  const syncText = behind === null ? 'not yet' : behind <= 2 ? `${sync.chain_tip.toLocaleString('en-US')} ✓` : `${behind} behind`;
  const agent = !o.unlocked ? [h('span', { class: 'dot bad' }), 'Locked']
    : o.paused ? [h('span', { class: 'dot bad' }), 'Stopped'] : [h('span', { class: 'dot good' }), 'Active'];
  const dark = document.documentElement.dataset.theme === 'dark';
  return h('aside', { class: 'side' },
    h('div', { class: 'brand' }, h('b', {}, 'Zec Agent Wallet'),
      h('span', { class: 'net' }, o.network === 'mainnet' ? 'Mainnet' : 'Testnet · test coins')),
    h('nav', { class: 'nav' }, tabs.map(([k, label]) => h('button', {
      class: 'ln' + (ui.screen === k ? ' active' : ''),
      onclick: () => { ui.screen = k; ui.approving = null; render(); },
    }, h('span', {}, label), k === 'bills' && needs > 0 ? h('span', { class: 'badge' }, String(needs)) : null))),
    h('div', { class: 'side-foot' },
      h('div', { class: 'status' },
        h('span', {}, 'Sync'), h('span', { class: 'mono small' }, syncText),
        h('span', {}, 'Keys'), h('span', {}, o.unlocked ? 'Unlocked' : 'Locked'),
        h('span', {}, 'Agent'), h('span', {}, agent)),
      h('button', {
        class: 'pill side-stop',
        onclick: act(async () => { await api('POST', '/settings', { paused: !o.paused }); await refresh(); },
          o.paused ? 'Agent payments resumed.' : 'Agent payments stopped. The agent cannot pay until you resume them.'),
      }, o.paused ? 'Resume agent payments' : 'Stop agent payments'),
      h('button', { class: 'theme', onclick: () => { applyTheme(dark ? 'light' : 'dark'); render(); } }, dark ? 'Light mode' : 'Dark mode')));
}

// ---- overview --------------------------------------------------------------

function limitBlock() {
  const o = cache.overview;
  const c = coin();
  const limit = o.daily_limit;
  const left = Math.max(0, limit - o.daily_used);
  if (ui.limitDraft === null) ui.limitDraft = zec(limit);
  const presets = c === 'ZEC' ? ['0', '0.0015', '0.005', '0.01'] : ['0', '0.01', '0.05', '0.1'];
  const input = h('input', { value: ui.limitDraft, inputmode: 'decimal', 'aria-label': 'Daily limit' });
  const numIn = h('div', { class: 'num-in' }, input, h('span', {}, c));
  const lim = {
    name: 'daily_limit', input, el: numIn, err: h('div', { class: 'ferr' }),
    empty: 'Type an amount, or 0 so the agent can pay nothing',
    check: v => amountProblem(v, c === 'ZEC' ? '0.005' : '0.05', true),
  };
  input.addEventListener('input', () => { ui.limitDraft = input.value; ui.limitSaved = false; setFieldError(lim, ''); });
  const meter = h('i');
  meter.style.width = limit ? `${Math.round((left / limit) * 100)}%` : '0%';
  const err = errorLine();
  const save = h('button', { class: 'pill ink save', type: 'submit' }, ui.limitSaved ? 'Saved ✓' : 'Save');
  const form = h('form', { class: 'controls', novalidate: true },
    presets.map(v => h('button', {
      class: 'chip' + (ui.limitDraft === v ? ' on' : ''), type: 'button',
      onclick: () => { ui.limitDraft = v; ui.limitSaved = false; render(); },
    }, v === '0' ? 'Off (0)' : `${v} ${c}`)),
    numIn, save);
  form.addEventListener('submit', (ev) => {
    ev.preventDefault();
    submit([lim], err, save, 'Saving…', async () => {
      const v = amount(input.value);
      const zero = Number(v) === 0;
      await api('POST', '/settings', { daily_limit_zec: zero ? '0' : v });
      ui.limitSaved = true;
      ui.limitDraft = null;
      flash('ok', zero ? 'Daily limit set to 0: the agent can pay nothing.' : `Daily limit saved: ${v} ${c} a day.`);
      await refresh();
    });
  });
  return h('div', {},
    h('div', { class: 'spread' }, h('span', { class: 'eyebrow' }, 'Daily limit · agent'), h('span', { class: 'small faint' }, 'resets every 24 h')),
    h('div', { class: 'big' }, h('b', {}, zec(left)), h('span', {}, `of ${zec(limit)} ${c} left today`),
      usd(left) ? h('span', { class: 'faint' }, `${usd(left)} left`) : null),
    h('div', { class: 'meter' }, meter),
    h('div', { class: 'help' }, 'Fees included. Even under the limit the agent pays only bills you approved.'),
    form, lim.err, err);
}

function needsBlock() {
  const c = coin();
  const waiting = (cache.bills || []).filter(b => state(b) === 'needs');
  const first = waiting[0];
  return h('div', {},
    h('div', { class: 'eyebrow' }, 'Needs your decision'),
    h('div', { class: 'big' }, h('b', {}, String(waiting.length)),
      h('span', {}, waiting.length === 1 ? 'bill waiting for you' : waiting.length ? 'bills waiting for you' : 'nothing right now')),
    first ? h('div', { class: 'first' },
      h('div', {},
        h('div', { class: 't' }, h('span', { class: 'dot' }), billName(first)),
        h('div', { class: 's' }, `${withUsd(`${zec(first.bill.amount)} ${c}`, first.bill.amount)} · ${first.contact ? first.contact.local_name : 'an unknown recipient'}`)),
      h('button', { class: 'pill accent', onclick: () => goApprove(first) }, 'Approve')) : null,
    first && overLimit(first) ? h('div', { class: 'note gap' }, 'Over the daily limit - raise it on the left') : null);
}

function miniBill(b) {
  return h('div', { class: 'mini ln' },
    h('span', {}, statusLabel(b)),
    h('span', {}, h('span', { class: 'title' }, billName(b)), h('span', { class: 'who' }, payeeText(b))),
    h('span', { class: 'mono' }, zec(b.bill.amount), usdTag(b.bill.amount)),
    h('span', { class: 'r' }, state(b) === 'needs' ? h('button', { class: 'pill outline xs', onclick: () => goApprove(b) }, 'Approve') : null));
}

function usdTag(zat) {
  const u = usd(zat);
  return u ? h('span', { class: 'usd' }, u) : null;
}

// Turning USD amounts on starts a price request every 10 minutes; say so.
function usdToggle(on) {
  return act(async () => {
    await api('POST', '/settings', { usd_prices: on });
    await refresh();
    if (on) setTimeout(() => refresh(true).catch(refreshFailed), 3000);
  }, on
    ? 'USD amounts on. The wallet asks Coinbase for the ZEC price every 10 minutes; Coinbase sees your IP address.'
    : 'USD amounts off. The wallet no longer asks for the price.');
}

async function openTopup() {
  ui.topup = 'loading';
  render();
  try {
    const r = await api('POST', '/topup');
    ui.topup = { address: r.address, qr: r.qr_svg };
  } catch (e) {
    ui.topup = null;
    if (!e.login) flash('bad', sentence(e.message));
  }
  render();
}

// A fresh address and its QR code. Only the owner sees it; the agent never does.
function topupPanel() {
  const t = ui.topup;
  const testnet = coin() !== 'ZEC';
  if (t === 'loading') return h('div', { class: 'topup' }, h('div', { class: 'muted' }, 'Making a fresh address…'));
  const copy = h('button', {
    class: 'pill ink sm', type: 'button',
    onclick: async () => {
      try { await navigator.clipboard.writeText(t.address); } catch (_) {
        flash('bad', 'Could not copy. Select the address and copy it by hand.');
        return;
      }
      flash('ok', 'Address copied.');
    },
  }, 'Copy address');
  return h('div', { class: 'topup' },
    h('img', { class: 'qr', src: 'data:image/svg+xml;utf8,' + encodeURIComponent(t.qr), alt: 'QR code of the address', width: 200, height: 200 }),
    h('div', {},
      h('div', { class: 'eyebrow' }, testnet ? 'Top up · test coins only' : 'Top up · send ZEC to this address'),
      h('div', { class: 'addr-big mono' }, t.address),
      h('div', { class: 'controls' }, copy,
        h('button', { class: 'pill ghost sm', type: 'button', onclick: openTopup }, 'New address'),
        h('button', { class: 'link', type: 'button', onclick: () => { ui.topup = null; render(); } }, 'Close')),
      h('ul', { class: 'steps' },
        testnet
          ? h('li', {}, 'Paste this address into a Zcash testnet faucet to get test coins (TAZ).')
          : [h('li', {}, 'Send from a shielded Zcash wallet such as ZODL or Zingo, or swap into ZEC with NEAR Intents and paste this address as the recipient.'),
            h('li', {}, 'Many exchanges cannot send to this kind of address (Orchard only). If yours refuses it, send to your own shielded wallet first.')],
        h('li', {}, 'The money first shows as pending and becomes spendable after 10 confirmations, about 12 minutes.'),
        h('li', {}, 'New address gives a fresh one each time, so payers cannot link your top-ups. Old addresses keep working.'),
        h('li', {}, 'Only you see this address; the agent never does.'))));
}

function renderOverview() {
  const o = cache.overview;
  const c = coin();
  const rank = b => (state(b) === 'needs' ? 0 : 1);
  const bills = [...(cache.bills || [])].sort((a, b) => rank(a) - rank(b) || b.bill.created_at - a.bill.created_at);
  const recent = activity().slice(0, 6);
  return h('div', { class: 'page home' },
    h('div', { class: 'hero' },
      h('div', { class: 'eyebrow' }, 'Balance · ready to spend'),
      h('div', { class: 'amount' }, h('b', {}, zec(o.balance ? o.balance.spendable : 0)), h('span', {}, c)),
      o.usd ? h('div', { class: 'usdline' }, usd(o.balance ? o.balance.spendable : 0),
        h('span', { class: 'faint' }, ` · 1 ZEC ${usd(1e8)}, ${o.usd.source} at ${clock(o.usd.at)}`))
        : o.usd_on ? h('div', { class: 'usdline faint' }, 'USD amounts are on; waiting for the ZEC price.') : null,
      h('div', { class: 'sub' }, `${zec(o.balance ? o.balance.pending : 0)} ${c} pending confirmation`),
      h('div', { class: 'hero-acts' },
        h('button', { class: 'pill outline sm', onclick: ui.topup ? () => { ui.topup = null; render(); } : openTopup }, ui.topup ? 'Hide top up' : 'Top up'),
        o.network === 'mainnet' && !o.usd_on ? h('button', { class: 'link', onclick: usdToggle(true) }, 'Show amounts in USD') : null),
      ui.topup ? topupPanel() : null,
      o.paused ? h('div', { class: 'notice' }, 'Agent payments are stopped. Payments already sent keep confirming; this cannot recall them.') : null,
      !o.unlocked ? h('div', { class: 'notice' }, 'The wallet is locked: the agent cannot pay until you unlock it in Settings.') : null),
    h('div', { class: 'duo ruled' }, limitBlock(), needsBlock()),
    h('div', { class: 'duo lists' },
      h('div', {},
        h('div', { class: 'colhead' }, h('b', {}, 'Bills'),
          h('div', {},
            h('button', { class: 'link', onclick: () => { ui.screen = 'bills'; render(); } }, 'View all'),
            h('button', { class: 'pill ink sm', onclick: () => { ui.screen = 'bills'; render(); } }, '+ Add bill'))),
        bills.length ? bills.slice(0, 5).map(miniBill) : h('div', { class: 'empty' }, 'No bills yet.')),
      h('div', {},
        h('div', { class: 'colhead' }, h('b', {}, 'Recent activity'),
          h('button', { class: 'link', onclick: () => { ui.screen = 'activity'; render(); } }, 'View all')),
        recent.length ? recent.map(r => h('div', { class: 'act ln' }, h('span', { class: 'tm' }, clock(r.at)), h('span', {}, r.text)))
          : h('div', { class: 'empty' }, 'Nothing yet.'))));
}

// ---- bills -----------------------------------------------------------------

function agentFields(view) {
  const rows = [];
  for (const [k, v] of Object.entries(view || {})) {
    const shown = v && typeof v === 'object' && 'zec' in v ? (v.usd_approx ? `${v.zec} (≈ $${v.usd_approx})` : v.zec) : String(v);
    rows.push(h('span', {}, k.replace(/_/g, ' ')), h('span', {}, shown));
  }
  return rows;
}

function senderText(b) {
  const parts = [];
  if (b.memo_text) parts.push(['Memo', b.memo_text]);
  if (b.bill.ext_label) parts.push(['Label', b.bill.ext_label]);
  if (b.bill.ext_message) parts.push(['Message', b.bill.ext_message]);
  if (!parts.length) return null;
  return [
    h('div', { class: 'small faint gap' }, 'From the sender. Never shown to the agent, never treated as instructions.'),
    parts.map(([k, v]) => h('div', { class: 'sender' }, h('b', {}, k + ': '), v)),
  ];
}

function trustLine(b) {
  if (b.contact && b.address_matches_contact) return h('div', { class: 'trust ok' }, `${b.contact.local_name}: the address matches one you saved for this contact`);
  if (b.contact) return h('div', { class: 'trust warn' }, `${b.contact.local_name}: assigned by you, but the address is not a saved one`);
  return h('div', { class: 'trust warn' }, 'Unknown recipient: this address is not saved for any contact');
}

function seesPanel(b) {
  const c = coin();
  const op = b.operation;
  const json = b.model_payment_view ? { bill: b.model_view, payment: b.model_payment_view } : b.model_view;
  return h('div', { class: 'panel' },
    h('div', {},
      h('div', { class: 'k' }, 'Only you see'),
      trustLine(b),
      senderText(b),
      op ? h('div', { class: 'kv' },
        h('span', {}, 'payment'), h('span', {}, op.state.replace(/_/g, ' ')),
        op.fee !== null ? h('span', {}, 'fee') : null, op.fee !== null ? h('span', {}, `${zec(op.fee)} ${c}`) : null,
        op.txid ? h('span', {}, 'txid') : null, op.txid ? h('span', { class: 'mono small' }, op.txid) : null) : null,
      h('details', {}, h('summary', {}, 'Recipient address and raw link'),
        h('div', { class: 'addr' }, b.bill.address),
        h('pre', {}, b.bill.uri))),
    h('div', {},
      h('div', { class: 'k' }, 'What the agent sees'),
      h('div', { class: 'kv' }, agentFields(b.model_view)),
      h('div', { class: 'small faint gap' }, 'Hidden: address, memo, sender note, txid'),
      h('pre', {}, JSON.stringify(json, null, 2))));
}

function approveForm(b) {
  const c = coin();
  const contacts = cache.contacts || [];
  // Prefilled with neutral text, never with the sender's label: that text must not reach the model.
  const label = field('label', 'Label the agent sees',
    h('input', { class: 'u', value: b.bill.label || `Bill for ${zec(b.bill.amount)} ${c}`, maxlength: 80, placeholder: 'e.g. Logo #17' }),
    { empty: 'Type a short label, for example "Logo #17"', hint: 'The agent finds the bill by this name. No addresses.' });
  const contact = field('contact', 'Contact',
    h('select', { class: 'u' },
      h('option', { value: '' }, 'No contact'),
      ...contacts.map(ct => h('option', { value: ct.id, selected: b.bill.contact_id === ct.id }, `${ct.local_name} (agent sees "${ct.alias}")`))),
    { optional: true, hint: 'Who gets the money; the agent sees only the alias.' });
  const fee = field('fee_limit', `Fee limit, ${c}`, h('input', { class: 'u mono', value: '0.0005', inputmode: 'decimal' }),
    { empty: 'Type the most the fee may be, for example 0.0005', check: v => amountProblem(v, '0.0005'), hint: 'The most the network fee may be. 0.0005 is plenty.' });
  const hours = field('hours', 'Valid for, hours', h('input', { class: 'u mono', value: '168', inputmode: 'numeric' }),
    {
      empty: 'Type how many hours the approval lasts, for example 168',
      check: v => (/^\d+$/.test(v) && Number(v) >= 1 && Number(v) <= 2160 ? '' : 'Whole hours from 1 to 2160 (90 days)'),
      hint: 'After this the agent can no longer pay it. 168 hours is a week.',
    });
  const pw = field('password', 'Wallet password', h('input', { class: 'u', type: 'password', autocomplete: 'current-password' }),
    { wide: true, empty: 'Type the wallet password to approve', hint: 'Needed for every approval, so the agent cannot approve anything itself.' });
  const err = errorLine();
  const ok = h('button', { class: 'pill ink', type: 'submit' }, withUsd(`Allow the agent to pay once: ${zec(b.bill.amount)} ${c}`, b.bill.amount));
  const form = h('form', { class: 'approve', novalidate: true },
    label.el, contact.el, fee.el, hours.el, pw.el,
    h('div', { class: 'wide' }, err),
    h('div', { class: 'btns wide' },
      ok,
      h('button', {
        class: 'pill bad', type: 'button',
        onclick: (ev) => submit([], err, ev.currentTarget, 'Rejecting…', async () => {
          await api('POST', `/bills/${b.bill.id}/reject`);
          ui.approving = null;
          flash('ok', 'Rejected. The agent cannot pay this bill.');
          await refresh();
        }),
      }, 'Reject'),
      h('button', { class: 'link', type: 'button', onclick: () => { ui.approving = null; render(); } }, 'Cancel')),
    h('p', { class: 'wide' }, 'This approves only this bill, with this recipient, amount and memo. It does not create a rule, raise a limit or change a contact.'));
  form.addEventListener('submit', (ev) => {
    ev.preventDefault();
    submit([label, contact, fee, hours, pw], err, ok, 'Checking the password…', async () => {
      const name = label.input.value.trim();
      await api('POST', `/bills/${b.bill.id}/approve`, {
        password: pw.input.value, label: name, contact_id: contact.input.value || null,
        fee_limit_zec: amount(fee.input.value), expires_hours: Number(hours.input.value),
      });
      pw.input.value = '';
      ui.approving = null;
      flash('ok', `Approved. The agent can pay “${name}” once.`);
      await refresh();
    });
  });
  setTimeout(() => pw.input.focus(), 0);
  return form;
}

function billRow(b) {
  const c = coin();
  const s = state(b);
  const id = b.bill.id;
  const open = ui.open.has(id);
  return h('div', { class: 'ln' },
    h('div', { class: 'trow' },
      h('span', {}, statusLabel(b)),
      h('span', {}, h('span', { class: 'title' }, billName(b)), h('span', { class: 'who' }, payeeText(b))),
      h('span', { class: 'when' }, whenText(b)),
      h('span', { class: 'r mono' }, zec(b.bill.amount), usdTag(b.bill.amount)),
      h('span', { class: 'acts' },
        s === 'needs' && ui.approving !== id ? h('button', { class: 'pill ink sm', onclick: () => { ui.approving = id; render(); } }, 'Approve') : null,
        s === 'approved' && b.approval ? h('button', {
          class: 'pill bad sm',
          onclick: act(async () => { await api('POST', `/approvals/${b.approval.id}/revoke`); await refresh(); },
            'Approval revoked. The agent can no longer pay this bill.'),
        }, 'Revoke') : null,
        h('button', {
          class: 'round', title: 'What the agent sees', 'aria-label': 'What the agent sees',
          onclick: () => { if (open) ui.open.delete(id); else ui.open.add(id); render(); },
        }, open ? '−' : '+'))),
    overLimit(b) ? h('div', { class: 'under' }, h('div', { class: 'note' }, `Over the ${zec(cache.overview.daily_limit)} ${c} daily limit - raise it on Overview`)) : null,
    s === 'needs' && ui.approving === id ? approveForm(b) : null,
    open ? seesPanel(b) : null);
}

function requestRow(r) {
  const c = coin();
  const m = r.model_view;
  const id = r.request.id;
  const open = ui.open.has(id);
  return h('div', { class: 'ln' },
    h('div', { class: 'req' },
      h('span', { class: m.state === 'paid' || m.state === 'overpaid' ? 'stat' : 'stat quiet' }, m.state.replace(/_/g, ' ')),
      h('span', {}, h('span', { class: 'title' }, r.request.label),
        h('span', { class: 'who' }, r.request.delivery === 'delivered_to_folder' ? 'delivered to the contact folder' : 'send the link to the payer yourself')),
      h('span', { class: 'r mono' }, zec(r.request.amount), usdTag(r.request.amount)),
      h('span', { class: 'r' }, h('button', { class: 'round', onclick: () => { if (open) ui.open.delete(id); else ui.open.add(id); render(); } }, open ? '−' : '+'))),
    open ? h('div', { class: 'panel' },
      h('div', {}, h('div', { class: 'k' }, 'Only you see'),
        h('div', { class: 'help' }, `Received ${m.detected.zec} ${c}, confirmed ${m.confirmed.zec} ${c}. "Paid" means the money arrived at this request's own address; it does not prove who sent it.`),
        h('pre', {}, r.request.uri)),
      h('div', {}, h('div', { class: 'k' }, 'What the agent sees'), h('pre', {}, JSON.stringify(m, null, 2)))) : null);
}

function addBillForm() {
  const prefix = coin() === 'ZEC' ? 'zcash:u1…' : 'zcash:utest1…';
  const button = h('button', { class: 'pill ink', type: 'submit' }, 'Add bill');
  const uri = field('uri', 'Payment link', h('input', { class: 'u mono', placeholder: `${prefix}?amount=0.005` }), {
    after: button,
    empty: 'Paste the payment link the payee sent you (it starts with zcash:)',
    check: v => (v.toLowerCase().startsWith('zcash:') ? '' : 'A payment link starts with “zcash:”'),
    hint: 'The link or QR text the payee sent you: one Orchard recipient and an amount.',
  });
  const err = errorLine();
  const form = h('form', { class: 'addbill', novalidate: true }, uri.el, err);
  form.addEventListener('submit', (ev) => {
    ev.preventDefault();
    submit([uri], err, button, 'Adding…', async () => {
      const res = await api('POST', '/bills/import', { uri: uri.input.value.trim() });
      uri.input.value = '';
      ui.filter = 'all';
      flash('ok', res.new ? 'Bill added. Approve it below if the agent should pay it.' : 'This bill is already in the list.');
      await refresh();
    });
  });
  return form;
}

function renderBills() {
  const c = coin();
  const bills = [...(cache.bills || [])].sort((a, b) => b.bill.created_at - a.bill.created_at);
  const count = k => k === 'all' ? bills.length : bills.filter(b => state(b) === k).length;
  const shown = ui.filter === 'all' ? bills : bills.filter(b => state(b) === ui.filter);
  const requests = cache.requests || [];
  return h('div', { class: 'page' },
    h('div', { class: 'head' },
      h('div', { class: 'eyebrow' }, 'Payments the agent can make'),
      h('h1', {}, 'Bills'),
      h('p', {}, 'Payment links you added or that arrived in the inbox. The agent pays an approved bill once, to the fixed address, before it expires.')),
    addBillForm(),
    h('div', { class: 'filters' }, [['all', 'All'], ['needs', 'Needs you'], ['approved', 'Approved'], ['paid', 'Paid']].map(([k, l]) =>
      h('button', { class: 'chip' + (ui.filter === k ? ' on' : ''), onclick: () => { ui.filter = k; render(); } }, `${l} · ${count(k)}`))),
    h('div', { class: 'table' },
      h('div', { class: 'trow th ln' }, h('span', {}, 'STATUS'), h('span', {}, 'BILL'), h('span', {}, 'EXPIRES / PAID'), h('span', { class: 'r' }, c), h('span', {})),
      shown.length ? shown.map(billRow) : h('div', { class: 'empty' }, 'No bills in this view.')),
    requests.length
      ? [h('div', { class: 'colhead gap-l' }, h('b', {}, 'Agent payment requests')), requests.map(requestRow)]
      : h('div', { class: 'foot' }, h('b', {}, 'Agent payment requests'), ' - none. The agent creates these when it needs to receive money.'));
}

// ---- activity --------------------------------------------------------------

function renderActivity() {
  const rows = activity();
  return h('div', { class: 'page' },
    h('div', { class: 'head' }, h('div', { class: 'eyebrow' }, 'Your actions and the agent’s'), h('h1', {}, 'Activity')),
    h('div', { class: 'log th ln' }, h('span', {}, 'WHEN'), h('span', {}, 'EVENT'), h('span', {}, 'BY'), h('span', { class: 'det' }, 'DETAIL')),
    rows.length ? rows.map(r => h('div', { class: 'log ln' },
      h('span', { class: 'tm' }, `${day(r.at)} ${clock(r.at)}`), h('span', {}, r.text), h('span', { class: 'by' }, r.who), h('span', { class: 'det' }, r.ref)))
      : h('div', { class: 'empty' }, 'Nothing yet.'));
}

// ---- settings --------------------------------------------------------------

function renderSettings() {
  const o = cache.overview;
  const prefix = coin() === 'ZEC' ? 'u1' : 'utest1';
  const name = field('local_name', 'Name', h('input', { class: 'u', placeholder: 'e.g. Anna, designer' }),
    { empty: 'Type a name for this contact', hint: 'Only you see it.' });
  const alias = field('alias', 'Alias the agent sees', h('input', { class: 'u', placeholder: 'e.g. designer' }),
    { optional: true, hint: 'Goes to the model provider. Left empty, the agent sees the name.' });
  const addr = field('addresses', 'Addresses', h('input', { class: 'u mono', placeholder: `${prefix}…` }), {
    optional: true, wide: true,
    check: v => (v.split(/[\s,]+/).filter(Boolean).every(a => a.startsWith(prefix)) ? '' : `Each address starts with ${prefix}; separate several with spaces`),
    hint: 'This contact’s unified addresses, separated by spaces. A bill to a saved address is marked as matching.',
  });
  const folder = field('delivery_dir', 'Folder for payment requests', h('input', { class: 'u' }),
    { optional: true, wide: true, hint: 'When the agent asks this contact for money, the request file is written here.' });
  const cErr = errorLine();
  const add = h('button', { class: 'pill ink wide', type: 'submit' }, 'Add contact');
  const contactForm = h('form', { class: 'cform', novalidate: true }, name.el, alias.el, addr.el, folder.el, h('div', { class: 'wide' }, cErr), add);
  contactForm.addEventListener('submit', (ev) => {
    ev.preventDefault();
    submit([name, alias, addr, folder], cErr, add, 'Adding…', async () => {
      await api('POST', '/contacts', {
        local_name: name.input.value.trim(), alias: alias.input.value.trim() || null, delivery_dir: folder.input.value.trim() || null,
        addresses: [...new Set(addr.input.value.split(/[\s,]+/).filter(Boolean))],
      });
      flash('ok', 'Contact added.');
      await refresh();
    });
  });
  const copy = h('button', {
    class: 'pill ghost sm',
    onclick: async () => {
      try { await navigator.clipboard.writeText('zaw mcp'); } catch (_) {
        flash('bad', 'Could not copy. Select the command and copy it by hand.');
        return;
      }
      ui.copied = true; render();
      setTimeout(() => { ui.copied = false; render(); }, 1500);
    },
  }, ui.copied ? 'Copied ✓' : 'Copy');
  return h('div', { class: 'page' },
    h('div', { class: 'head' }, h('div', { class: 'eyebrow' }, 'Daily limit is on Overview'), h('h1', {}, 'Settings')),
    h('div', { class: 'set' }, h('b', {}, 'Contacts'),
      h('div', {},
        h('div', { class: 'muted' }, 'The agent sees only the alias, and it goes to the model provider. Name and addresses stay on this device.'),
        h('div', { class: 'ctab' },
          h('div', { class: 'crow th' }, h('span', {}, 'NAME'), h('span', {}, 'ALIAS · AGENT'), h('span', {}, 'ADDR.')),
          (cache.contacts || []).length ? cache.contacts.map(ct => h('div', { class: 'crow ln' },
            h('span', {}, ct.local_name), h('span', { class: 'mono muted' }, ct.alias), h('span', { class: 'muted' }, String(ct.addresses.length))))
            : h('div', { class: 'empty' }, 'No contacts yet.')),
        contactForm)),
    h('div', { class: 'set three' }, h('b', {}, 'Amounts in USD'),
      h('span', { class: 'muted' }, o.network === 'mainnet'
        ? 'Approximate dollar amounts for you and the agent. While on, the wallet asks Coinbase (Kraken if Coinbase fails) for the ZEC price every 10 minutes, on a timer, never at payment time; they see your IP address. Bills, approvals and limits stay in ZEC.'
        : 'Mainnet only: test coins have no price.'),
      o.network === 'mainnet'
        ? h('button', { class: o.usd_on ? 'pill ghost' : 'pill ink', onclick: usdToggle(!o.usd_on) }, o.usd_on ? 'Turn off' : 'Turn on')
        : h('span', {})),
    h('div', { class: 'set three' }, h('b', {}, 'Lock wallet'),
      h('span', { class: 'muted' }, 'Clears the spending key from memory. No payments until you unlock.'),
      o.unlocked
        ? h('button', {
          class: 'pill bad',
          onclick: act(async () => { await api('POST', '/lock'); await refresh(); }, 'Wallet locked. The agent cannot pay until you unlock it.'),
        }, 'Lock')
        : h('button', { class: 'pill ink', onclick: () => renderLogin() }, 'Unlock')),
    h('div', { class: 'set' }, h('b', {}, 'Connect an agent'),
      h('div', {},
        h('div', { class: 'muted' }, 'MCP server for Claude Desktop or Claude Code. It holds no keys and reads its access token from the wallet folder. For your own agent on the Claude API, see examples/claude_agent.py.'),
        h('div', { class: 'cmd' }, h('code', {}, 'zaw mcp'), copy))));
}

// ---- main ------------------------------------------------------------------

async function refresh(auto) {
  const [overview, bills, requests, contacts, events] = await Promise.all([
    api('GET', '/overview'), api('GET', '/bills'), api('GET', '/requests'), api('GET', '/contacts'), api('GET', '/events'),
  ]);
  cache = { overview, bills, requests, contacts, events };
  render(auto);
}

// A form with typed text or a shown error, which a background refresh must not wipe.
function formInUse(root) {
  if (root.querySelector('.ferr:not(:empty), .err:not(:empty)')) return true;
  return [...root.querySelectorAll('input, textarea')].some(el => el.value !== el.defaultValue)
    || [...root.querySelectorAll('select')].some(s => [...s.options].some(o => o.selected !== o.defaultSelected));
}

function render(auto) {
  if (!cache.overview) return;
  const root = document.getElementById('root');
  // Keep typed text: background refreshes do not re-render while a form is being filled.
  if (auto && document.activeElement && root.contains(document.activeElement) && ['INPUT', 'TEXTAREA', 'SELECT'].includes(document.activeElement.tagName)) return;
  if (auto && (ui.approving || formInUse(root))) return;
  const views = { overview: renderOverview, bills: renderBills, activity: renderActivity, settings: renderSettings };
  const main = h('main', {}, views[ui.screen]());
  root.replaceChildren(h('div', { class: 'app' }, renderSide(), main));
}

// A failed refresh says so; otherwise the window would quietly show old data.
function refreshFailed(e) {
  if (!e.login) flash('bad', sentence(e.message));
}

function start() {
  refresh().catch(refreshFailed);
  if (!window.__poll) window.__poll = setInterval(() => { if (csrf) refresh(true).catch(refreshFailed); }, 10000);
}

(async () => {
  try {
    const s = await (await fetch('/api/owner/session')).json();
    if (s.logged_in && s.csrf) { csrf = s.csrf; start(); } else renderLogin();
  } catch (_) { renderLogin(); }
})();
