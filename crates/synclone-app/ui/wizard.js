// "Add cloud storage" (PLAN.md §6.2): pick, sign in, name, done.
const FEATURED = ['drive', 'onedrive', 'drime'];
let service = null; // what drive.service_options returned
let created = null; // the drive once added
let signinUrl = null;

function step(name) {
  for (const s of ['pick', 'other', 'signin', 'name', 'done']) show(`step-${s}`, s === name);
}

function fail(id, e) {
  $(id).textContent = errorText(e);
  show(id, true);
}

async function loadServices() {
  let all = [];
  try {
    all = await rpc('drive.services', { advanced: await invoke('advanced_mode') });
  } catch (e) {
    $('services').replaceChildren(el('div', { class: 'error', text: errorText(e) }));
    return;
  }
  const others = all.filter((s) => !FEATURED.includes(s.type));
  const render = () => {
    const q = $('search').value.trim().toLowerCase();
    const hits = others.filter(
      (s) => !q || s.name.toLowerCase().includes(q) || (s.description || '').toLowerCase().includes(q)
    );
    $('services').replaceChildren(
      ...(hits.length
        ? hits.map((s) =>
            el('button', { 'data-type': s.type, onclick: () => choose(s.type) }, s.name, el('small', { text: s.description }))
          )
        : [el('div', { class: 'empty', text: 'No service matches.' })])
    );
  };
  $('search').oninput = render;
  render();
}

function label(name) {
  const t = name.replace(/_/g, ' ');
  return t.charAt(0).toUpperCase() + t.slice(1);
}

function field(o) {
  const id = `opt-${o.name}`;
  const examples = (o.examples || []).filter((x) => x.value !== '');
  let input;
  if (examples.length > 1 && examples.length <= 30) {
    input = el('select', { id, 'data-option': o.name });
    if (!o.required) input.append(el('option', { value: '', text: '' }));
    for (const x of examples) {
      const opt = el('option', { value: x.value, text: x.help ? `${x.value} — ${x.help}` : x.value });
      if (String(o.default) === x.value) opt.selected = true;
      input.append(opt);
    }
  } else {
    input = el('input', {
      id,
      'data-option': o.name,
      type: o.password ? 'password' : 'text',
      autocomplete: 'off',
      spellcheck: 'false',
      placeholder: o.default !== '' && o.default !== null && o.default !== undefined ? String(o.default) : '',
    });
  }
  return el('div', {}, el('label', { for: id, text: label(o.name) + (o.required ? '' : ' (optional)') }), input, el('div', { class: 'hint', text: o.help || '' }));
}

async function choose(type) {
  try {
    service = await rpc('drive.service_options', { type });
  } catch (e) {
    alert(errorText(e));
    return;
  }
  $('signin-title').textContent = `Sign in to ${service.name}`;
  $('signin-lead').textContent = service.description || '';
  show('browser-box', service.auth === 'browser');
  show('token-box', service.auth === 'token');
  show('signin', service.auth === 'browser');
  show('connect', service.auth !== 'browser');
  show('waiting', false);
  show('signin-error', false);
  $('token').value = '';
  show('token-help', !!service.token_help_url);
  const main = service.options.filter((o) => !o.advanced && service.auth === 'form');
  const adv = service.options.filter((o) => o.advanced || service.auth !== 'form');
  $('form-box').replaceChildren(...main.map(field));
  $('advanced').replaceChildren(...adv.map(field));
  show('advanced-box', adv.length > 0);
  step('signin');
}

function collect() {
  const params = {};
  for (const i of document.querySelectorAll('[data-option]')) {
    const v = i.value.trim();
    if (v !== '') params[i.dataset.option] = v;
  }
  if (service.auth === 'token') params[service.token_option] = $('token').value.trim();
  return params;
}

async function connect() {
  show('signin-error', false);
  const params = collect();
  const missing = service.options.filter((o) => o.required && !o.advanced && !params[o.name]);
  if (service.auth === 'token' && !params[service.token_option]) {
    fail('signin-error', 'Paste your access token first.');
    return;
  }
  if (missing.length) {
    fail('signin-error', `Please fill in: ${missing.map((o) => o.name.replace(/_/g, ' ')).join(', ')}`);
    return;
  }
  for (const b of ['signin', 'connect', 'signin-back']) $(b).disabled = true;
  if (service.auth === 'browser') {
    show('waiting', true);
    show('signin-cancel', true);
  }
  try {
    const name = (await rpc('drive.suggest_name', { type: service.type })).name;
    created = await invoke('add_drive', { params: { type: service.type, name, params } });
    $('name').value = created.name;
    step('name');
    $('name').focus();
  } catch (e) {
    fail('signin-error', e);
  } finally {
    for (const b of ['signin', 'connect', 'signin-back']) $(b).disabled = false;
    show('waiting', false);
    show('signin-cancel', false);
  }
}

async function finish() {
  show('name-error', false);
  const name = $('name').value.trim();
  try {
    if (name && name !== created.name) {
      created.name = (await rpc('drive.rename', { drive_id: created.id, new_name: name })).name;
    }
    const drives = await rpc('drive.list');
    const d = drives.find((x) => x.id === created.id);
    created.folder = d ? d.folder : created.folder;
  } catch (e) {
    fail('name-error', e);
    return;
  }
  $('done-text').textContent = `${created.name} is connected. Its folder is ready.`;
  step('done');
  invoke('open_path', { path: created.folder }).catch(() => {});
}

window.__TAURI__.event.listen('signin-url', (ev) => {
  signinUrl = ev.payload;
});

for (const t of document.querySelectorAll('.tile[data-type]')) t.onclick = () => choose(t.dataset.type);
$('other-tile').onclick = () => {
  step('other');
  loadServices();
  $('search').focus();
};
$('other-back').onclick = () => step('pick');
$('signin-back').onclick = () => step('pick');
$('signin').onclick = connect;
$('connect').onclick = connect;
$('signin-cancel').onclick = () => rpc('drive.cancel_signin').catch(() => {});
$('signin-again').onclick = () => signinUrl && invoke('open_url', { url: signinUrl });
$('token-help').onclick = () => invoke('open_url', { url: service.token_help_url });
$('finish').onclick = finish;
$('name').onkeydown = (e) => {
  // Returning false from a handler would cancel every other keystroke.
  if (e.key === 'Enter') finish();
};
$('done-folder').onclick = () => invoke('open_path', { path: created.folder });
$('done-close').onclick = () => invoke('close_self');
$('add-another').onclick = () => {
  created = null;
  step('pick');
};
step('pick');
