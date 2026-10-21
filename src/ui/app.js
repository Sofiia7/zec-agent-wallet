'use strict';

// Owner window. All text that came from outside (memos, labels, messages, names)
// is inserted with textContent only.

let csrf = null;
let tab = 'overview';
let cache = {};

function h(tag, attrs, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === null || v === undefined || v === false) continue;
    if (k === 'class') el.className = v;
    else if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
    else el.setAttribute(k, v === true ? '' : v);
  }
  for (const c of children.flat()) {
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
  const res = await fetch('/api/owner' + path, opts);
  let data = null;
  try { data = await res.json(); } catch (_) { data = null; }
  if (res.status === 401 && path !== '/login') { csrf = null; renderLogin(); throw new Error('login required'); }
  if (!res.ok) throw new Error((data && data.error) || ('request failed: ' + res.status));
  return data;
}

function zec(zat) {
  if (zat === null || zat === undefined) return '-';
  const whole = Math.floor(zat / 1e8);
  const frac = String(zat % 1e8).padStart(8, '0').replace(/0+$/, '');
  return frac ? `${whole}.${frac}` : String(whole);
}

function when(unix) {
  return new Date(unix * 1000).toLocaleString();
}

function coin() {
  return cache.overview && cache.overview.network === 'mainnet' ? 'ZEC' : 'TAZ';
}

function modelPanel(title, value, note) {
  return h('div', { class: 'model' },
    h('h3', {}, title),
    h('p', { class: 'muted' }, note || 'Exactly what the agent API returns for this item. This is what reaches the model and its provider.'),
    h('pre', {}, JSON.stringify(value, null, 2)));
}

function errorLine() { return h('div', { class: 'error', role: 'alert' }); }

async function withError(errEl, fn) {
  errEl.textContent = '';
  try { await fn(); } catch (e) { errEl.textContent = e.message; }
}

// ---- login -----------------------------------------------------------------

function renderLogin() {
  document.getElementById('tabs').hidden = true;
  document.getElementById('pause').hidden = true;
  const app = document.getElementById('app');
  const pw = h('input', { type: 'password', id: 'pw', autocomplete: 'current-password' });
  const err = errorLine();
  const form = h('form', { class: 'card login' },
    h('h2', {}, 'Unlock the wallet'),
    h('p', { class: 'muted' }, 'The password decrypts the seed. While unlocked, the agent can pay bills you approved; nothing else.'),
    h('label', { for: 'pw' }, 'Password'), pw, err,
    h('button', { class: 'primary', type: 'submit' }, 'Unlock'));
  form.addEventListener('submit', (ev) => {
    ev.preventDefault();
    withError(err, async () => {
      const res = await api('POST', '/login', { password: pw.value });
      csrf = res.csrf;
      pw.value = '';
      start();
    });
  });
  app.replaceChildren(form);
  pw.focus();
}

// ---- header ----------------------------------------------------------------

function renderHeader() {
  const o = cache.overview;
  if (!o) return;
  const net = document.getElementById('net');
  net.textContent = o.network === 'mainnet' ? 'MAINNET' : 'TESTNET - test coins only';
  const sync = o.sync;
  const behind = sync ? sync.chain_tip - sync.fully_scanned : null;
  const chips = [
    behind === null ? h('span', { class: 'chip warn' }, 'Not synced yet')
      : behind <= 2 ? h('span', { class: 'chip ok' }, `Synced, block ${sync.chain_tip}`)
        : h('span', { class: 'chip warn' }, `Syncing, ${behind} blocks behind`),
    o.unlocked ? h('span', { class: 'chip ok' }, 'Unlocked') : h('span', { class: 'chip bad' }, 'Locked'),
    o.paused ? h('span', { class: 'chip bad' }, 'Agent payments stopped') : h('span', { class: 'chip ok' }, 'Agent payments on'),
  ];
  if (o.needs_approval > 0) chips.push(h('span', { class: 'chip warn' }, `${o.needs_approval} waiting for you`));
  document.getElementById('chips').replaceChildren(...chips);
  const pause = document.getElementById('pause');
  pause.hidden = false;
  pause.textContent = o.paused ? 'Resume agent payments' : 'Stop agent payments';
  pause.className = o.paused ? '' : 'danger';
  pause.onclick = async () => {
    await api('POST', '/settings', { paused: !o.paused });
    await refresh();
  };
}

// ---- overview --------------------------------------------------------------

function renderOverview() {
  const o = cache.overview;
  const c = coin();
  const left = Math.max(0, o.daily_limit - o.daily_used);
  const waiting = (cache.bills || []).filter(b => b.bill.status === 'needs_approval');
  return [
    h('div', { class: 'grid' },
      h('div', { class: 'card' }, h('h3', {}, 'Wallet'),
        h('div', { class: 'big' }, `${zec(o.balance ? o.balance.spendable : 0)} ${c}`),
        h('div', { class: 'muted num' }, `spendable, ${zec(o.balance ? o.balance.pending : 0)} ${c} pending confirmation`)),
      h('div', { class: 'card' }, h('h3', {}, 'The agent may still spend today'),
        h('div', { class: 'big' }, `${zec(left)} ${c}`),
        h('div', { class: 'muted num' }, `of a ${zec(o.daily_limit)} ${c} daily limit, and only on bills you approved`)),
      h('div', { class: 'card' }, h('h3', {}, 'Needs your decision'),
        h('div', { class: 'big' }, String(waiting.length)),
        h('div', { class: 'muted' }, waiting.length ? 'bills waiting in Payments' : 'nothing right now'))),
    o.paused ? h('div', { class: 'notice bad' }, 'Agent payments are stopped. Payments already sent keep confirming; this cannot recall them.') : null,
    modelPanel('What the model gets from wallet_status', o.agent_view),
  ];
}

// ---- payments --------------------------------------------------------------

function trustLine(b) {
  if (b.contact && b.address_matches_contact) return h('span', { class: 'ok' }, `${b.contact.local_name}: address matches a saved address of this contact`);
  if (b.contact) return h('span', { class: 'warn' }, `${b.contact.local_name}: assigned by you, the address is not a saved one`);
  return h('span', { class: 'warn' }, 'Unknown recipient: this address is not saved for any contact');
}

function senderText(b) {
  const parts = [];
  if (b.memo_text) parts.push(['Memo', b.memo_text]);
  if (b.bill.ext_label) parts.push(['Label', b.bill.ext_label]);
  if (b.bill.ext_message) parts.push(['Message', b.bill.ext_message]);
  if (!parts.length) return null;
  return h('div', {},
    h('label', {}, 'Text from the sender. Never shown to the model, never treated as instructions.'),
    ...parts.map(([k, v]) => h('div', { class: 'sender' }, h('strong', {}, k + ': '), v)));
}

function approveForm(b) {
  const c = coin();
  const contacts = cache.contacts || [];
  const label = h('input', { value: b.bill.label || '', maxlength: 80, placeholder: 'e.g. Logo #17' });
  const contact = h('select', {},
    h('option', { value: '' }, '- no contact -'),
    ...contacts.map(ct => h('option', { value: ct.id, selected: b.bill.contact_id === ct.id }, `${ct.local_name} (model sees "${ct.alias}")`)));
  const fee = h('input', { value: '0.0005', inputmode: 'decimal' });
  const hours = h('input', { value: '168', inputmode: 'numeric' });
  const pw = h('input', { type: 'password', autocomplete: 'current-password' });
  const err = errorLine();
  const form = h('form', {},
    h('label', {}, 'Label the model will see'), label,
    h('label', {}, 'Contact'), contact,
    h('div', { class: 'row' },
      h('div', { class: 'grow' }, h('label', {}, `Fee limit, ${c}`), fee),
      h('div', { class: 'grow' }, h('label', {}, 'Valid for, hours'), hours)),
    h('label', {}, 'Password'), pw, err,
    h('div', { class: 'row' },
      h('button', { class: 'primary', type: 'submit' }, `Allow the agent to pay this bill once: ${zec(b.bill.amount)} ${c}`),
      h('button', { type: 'button', class: 'danger', onclick: () => withError(err, async () => { await api('POST', `/bills/${b.bill.id}/reject`); await refresh(); }) }, 'Reject')),
    h('p', { class: 'muted' }, 'This approves only this bill, with this recipient, amount and memo. It does not create a rule, raise a limit or change a contact.'));
  form.addEventListener('submit', (ev) => {
    ev.preventDefault();
    withError(err, async () => {
      await api('POST', `/bills/${b.bill.id}/approve`, {
        password: pw.value, label: label.value, contact_id: contact.value || null,
        fee_limit_zec: fee.value, expires_hours: Number(hours.value) || 168,
      });
      pw.value = '';
      await refresh();
    });
  });
  return form;
}

function billCard(b) {
  const c = coin();
  const op = b.operation;
  const status = b.bill.status;
  const statusText = {
    needs_approval: b.approval_requested ? 'The agent asked you to approve this bill' : 'Waiting for your decision',
    approved: 'Approved: the agent may pay it once',
    paying: 'Payment in progress',
    paid: 'Paid',
    rejected: 'Rejected',
    failed: 'Payment failed before anything was sent',
    expired_unpaid: 'Transaction expired unpaid',
  }[status] || status;
  const cls = status === 'paid' ? 'ok' : (status === 'failed' || status === 'rejected') ? 'bad' : 'warn';
  return h('div', { class: 'card' },
    h('div', { class: 'split' },
      h('div', {},
        h('div', { class: 'row between' },
          h('span', { class: 'big' }, `${zec(b.bill.amount)} ${c}`),
          h('span', { class: `status ${cls}` }, statusText)),
        h('p', {}, trustLine(b)),
        b.bill.label ? h('p', {}, h('strong', {}, 'Label: '), b.bill.label) : null,
        op ? h('p', { class: 'num' }, `Payment ${op.state.replace(/_/g, ' ')}`, op.fee !== null ? `, fee ${zec(op.fee)} ${c}` : '', op.txid ? h('div', { class: 'addr' }, `txid ${op.txid}`) : null) : null,
        senderText(b),
        h('details', {}, h('summary', {}, 'Recipient address and raw link'),
          h('div', { class: 'addr' }, b.bill.address),
          h('pre', {}, b.bill.uri)),
        status === 'needs_approval' ? approveForm(b) : null,
        status === 'approved' && b.approval ? h('button', { class: 'danger', onclick: async () => { await api('POST', `/approvals/${b.approval.id}/revoke`); await refresh(); } }, 'Revoke approval') : null),
      h('div', {},
        modelPanel('What the model gets', b.model_payment_view ? { bill: b.model_view, payment: b.model_payment_view } : b.model_view,
          'Exactly what list_bills and pay_bill return. No address, memo, sender text or txid.'))));
}

function importForm() {
  const uri = h('textarea', { placeholder: 'zcash:utest1...?amount=0.005' });
  const err = errorLine();
  const form = h('form', { class: 'card' },
    h('h3', {}, 'Add a bill'),
    h('p', { class: 'muted' }, 'Paste a ZIP 321 payment link. Only single-recipient links to a unified address with an Orchard receiver are accepted.'),
    uri, err, h('button', { type: 'submit' }, 'Add bill'));
  form.addEventListener('submit', (ev) => {
    ev.preventDefault();
    withError(err, async () => { await api('POST', '/bills/import', { uri: uri.value }); uri.value = ''; await refresh(); });
  });
  return form;
}

function requestRow(r) {
  const c = coin();
  const m = r.model_view;
  return h('div', { class: 'card' },
    h('div', { class: 'split' },
      h('div', {},
        h('div', { class: 'row between' },
          h('span', { class: 'big' }, `${zec(r.request.amount)} ${c}`),
          h('span', { class: 'status ' + (m.state === 'paid' || m.state === 'overpaid' ? 'ok' : 'warn') }, m.state.replace(/_/g, ' '))),
        h('p', {}, h('strong', {}, 'Label: '), r.request.label),
        h('p', { class: 'muted' }, `Received ${m.detected.zec} ${c}, confirmed ${m.confirmed.zec} ${c}. "Paid" means the money arrived at this bill's own address; it does not prove who sent it.`),
        h('label', {}, r.request.delivery === 'delivered_to_folder' ? 'Delivered to the contact folder. Link:' : 'Send this link to the payer yourself:'),
        h('pre', {}, r.request.uri)),
      modelPanel('What the model gets', m)));
}

function renderPayments() {
  const bills = cache.bills || [];
  const requests = cache.requests || [];
  const events = cache.events || [];
  return [
    h('h2', {}, 'Bills to pay'), importForm(),
    bills.length ? bills.map(billCard) : h('p', { class: 'muted' }, 'No bills yet.'),
    h('h2', {}, 'My payment requests'),
    requests.length ? requests.map(requestRow) : h('p', { class: 'muted' }, 'No requests yet. The agent creates them with request_payment.'),
    h('h2', {}, 'History'),
    h('div', { class: 'card' }, h('table', {},
      h('tr', {}, h('th', {}, 'When'), h('th', {}, 'Event'), h('th', {}, 'Item')),
      events.map(e => h('tr', {}, h('td', { class: 'num' }, when(e.at)), h('td', {}, e.kind.replace(/_/g, ' ')), h('td', { class: 'addr' }, e.ref_id || ''))))),
  ];
}

// ---- approvals -------------------------------------------------------------

function renderApprovals() {
  const c = coin();
  const bills = (cache.bills || []).filter(b => b.approval);
  if (!bills.length) return [h('p', { class: 'muted' }, 'No approvals yet. Approve a bill in Payments.')];
  return bills.map(b => {
    const a = b.approval;
    const who = b.contact ? b.contact.local_name : 'an unknown recipient';
    const sentence = `The agent may pay bill "${b.bill.label || b.bill.id}" to ${who} once, ${zec(b.bill.amount)} ${c}, until ${when(a.expires_at)}. Fee at most ${zec(a.fee_limit)} ${c}. Recipient fixed.`;
    return h('div', { class: 'card' },
      h('div', { class: 'row between' }, h('strong', {}, a.status.replace(/_/g, ' ')),
        a.status === 'active' ? h('button', { class: 'danger', onclick: async () => { await api('POST', `/approvals/${a.id}/revoke`); await refresh(); } }, 'Revoke') : null),
      h('p', {}, sentence));
  });
}

// ---- settings --------------------------------------------------------------

function renderSettings() {
  const c = coin();
  const o = cache.overview;
  const limit = h('input', { value: zec(o.daily_limit), inputmode: 'decimal' });
  const limitErr = errorLine();
  const limitForm = h('form', { class: 'card' },
    h('h3', {}, 'Daily limit'),
    h('p', { class: 'muted' }, `A safety net on top of approvals: the agent never pays more than this in 24 hours, fees included. 0 means the agent pays nothing.`),
    h('label', {}, `Limit, ${c}`), limit, limitErr, h('button', { type: 'submit' }, 'Save'));
  limitForm.addEventListener('submit', (ev) => {
    ev.preventDefault();
    withError(limitErr, async () => { await api('POST', '/settings', { daily_limit_zec: limit.value }); await refresh(); });
  });

  const name = h('input', {});
  const alias = h('input', { placeholder: 'defaults to the name' });
  const folder = h('input', { placeholder: 'optional, for the demo' });
  const addrs = h('textarea', { placeholder: 'one unified address per line (optional)' });
  const cErr = errorLine();
  const contactForm = h('form', {},
    h('label', {}, 'Name (only you see it)'), name,
    h('label', {}, 'Alias the model sees'), alias,
    h('label', {}, 'Folder to deliver payment requests to'), folder,
    h('label', {}, 'Saved addresses'), addrs, cErr,
    h('button', { type: 'submit' }, 'Add contact'));
  contactForm.addEventListener('submit', (ev) => {
    ev.preventDefault();
    withError(cErr, async () => {
      await api('POST', '/contacts', {
        local_name: name.value, alias: alias.value || null, delivery_dir: folder.value || null,
        addresses: addrs.value.split('\n').map(s => s.trim()).filter(Boolean),
      });
      name.value = alias.value = folder.value = addrs.value = '';
      await refresh();
    });
  });

  return [
    limitForm,
    h('div', { class: 'card' }, h('h3', {}, 'Contacts'),
      h('table', {}, h('tr', {}, h('th', {}, 'Name'), h('th', {}, 'Model sees'), h('th', {}, 'Saved addresses')),
        (cache.contacts || []).map(ct => h('tr', {}, h('td', {}, ct.local_name), h('td', {}, ct.alias), h('td', { class: 'num' }, String(ct.addresses.length))))),
      h('p', { class: 'muted' }, 'The alias goes to the model provider. Use something you are comfortable sharing.'),
      contactForm),
    h('div', { class: 'card' }, h('h3', {}, 'Lock'),
      h('p', { class: 'muted' }, 'Locking removes the spending key from memory. The agent cannot pay until you unlock again.'),
      h('button', { class: 'danger', onclick: async () => { await api('POST', '/lock'); await refresh(); } }, 'Lock the wallet')),
    h('div', { class: 'card' }, h('h3', {}, 'Connect an agent'),
      h('p', { class: 'muted' }, 'Add this MCP server to Claude Desktop or Claude Code. It holds no keys and reads its access token from the wallet folder.'),
      h('pre', {}, 'zaw mcp')),
  ];
}

// ---- main ------------------------------------------------------------------

async function refresh() {
  const [overview, bills, requests, contacts, events] = await Promise.all([
    api('GET', '/overview'), api('GET', '/bills'), api('GET', '/requests'), api('GET', '/contacts'), api('GET', '/events'),
  ]);
  cache = { overview, bills, requests, contacts, events };
  render();
}

function render() {
  renderHeader();
  const views = { overview: renderOverview, payments: renderPayments, approvals: renderApprovals, settings: renderSettings };
  const app = document.getElementById('app');
  // Keep typed text: do not re-render while the owner is filling a form.
  if (document.activeElement && app.contains(document.activeElement) && ['INPUT', 'TEXTAREA', 'SELECT'].includes(document.activeElement.tagName)) return;
  app.replaceChildren(...[views[tab]()].flat());
}

function start() {
  const tabs = document.getElementById('tabs');
  tabs.hidden = false;
  tabs.querySelectorAll('button').forEach(b => {
    b.onclick = () => {
      tab = b.dataset.tab;
      tabs.querySelectorAll('button').forEach(x => x.classList.toggle('active', x === b));
      render();
    };
  });
  refresh().catch(() => {});
  if (!window.__poll) window.__poll = setInterval(() => { if (csrf) refresh().catch(() => {}); }, 10000);
}

(async () => {
  try {
    const s = await (await fetch('/api/owner/session')).json();
    if (s.logged_in && s.csrf) { csrf = s.csrf; start(); } else renderLogin();
  } catch (_) { renderLogin(); }
})();
