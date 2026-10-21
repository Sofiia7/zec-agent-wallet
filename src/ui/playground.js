'use strict';

let lastRequest = null;
let lastOperation = null;

async function post(path, body) {
  const res = await fetch(path, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body || {}),
  });
  let data = null;
  try { data = await res.json(); } catch (_) { data = { error: 'no response' }; }
  return data;
}

function show(id, value) {
  document.getElementById(id).textContent = JSON.stringify(value, null, 2);
}

async function refreshBills() {
  const bills = await post('/api/playground/agent/list_bills');
  if (!Array.isArray(bills)) return bills;
  const select = document.getElementById('bill');
  const current = select.value;
  select.replaceChildren(new Option('pick a bill', ''));
  for (const b of bills) {
    const text = `${b.payee} ${b.amount.zec} TAZ ${b.status}`;
    select.append(new Option(text, b.bill_id));
  }
  if ([...select.options].some(o => o.value === current)) select.value = current;
  else if (bills.length) select.value = bills[0].bill_id;
  return bills;
}

document.getElementById('issue').onclick = async () => {
  show('designer-out', 'working...');
  const r = await post('/api/playground/designer/issue');
  if (r.model_receives && r.model_receives.request_id) lastRequest = r.model_receives.request_id;
  show('designer-out', r);
  await refreshBills();
};

document.getElementById('check').onclick = async () => {
  if (!lastRequest) { show('designer-out', 'Issue a bill first.'); return; }
  show('designer-out', await post(`/api/playground/designer/check/${lastRequest}`));
};

document.getElementById('forge').onclick = async () => {
  show('attacker-out', await post('/api/playground/attacker/forge'));
  await refreshBills();
};

for (const button of document.querySelectorAll('button[data-tool]')) {
  button.onclick = async () => {
    const tool = button.dataset.tool;
    const args = {};
    if (button.dataset.needs === 'bill') {
      args.bill_id = document.getElementById('bill').value;
      if (!args.bill_id) { show('agent-out', 'Pick a bill first (list_bills).'); return; }
    }
    if (button.dataset.needs === 'op') {
      if (!lastOperation) { show('agent-out', 'Call pay_bill first.'); return; }
      args.operation_id = lastOperation;
    }
    document.getElementById('agent-call').textContent = `${tool}(${JSON.stringify(args)})`;
    const result = tool === 'list_bills' ? await refreshBills() : await post(`/api/playground/agent/${tool}`, args);
    if (result && result.operation_id) lastOperation = result.operation_id;
    show('agent-out', result);
  };
}

refreshBills();
