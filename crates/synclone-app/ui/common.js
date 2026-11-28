// Shared helpers for every window. User-visible text lives in the pages;
// tests check it for rclone vocabulary (PLAN.md §6.2).
const invoke = window.__TAURI__.core.invoke;

function rpc(method, params) {
  return invoke('rpc', { method, params: params || {} });
}

function $(id) {
  return document.getElementById(id);
}

function el(tag, attrs, ...children) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (k === 'onclick' || k === 'oninput' || k === 'onchange') e[k] = v;
    else if (k === 'text') e.textContent = v;
    else if (v !== undefined && v !== null && v !== false) e.setAttribute(k, v === true ? '' : v);
  }
  for (const c of children) e.append(c);
  return e;
}

function show(id, on) {
  $(id).classList.toggle('hidden', !on);
}

function size(n) {
  if (n === null || n === undefined || n < 0) return '';
  const u = ['bytes', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  let x = n;
  while (x >= 1024 && i < u.length - 1) { x /= 1024; i++; }
  return i === 0 ? `${x} ${u[0]}` : `${x.toFixed(x < 10 ? 1 : 0)} ${u[i]}`;
}

function when(ms) {
  return ms ? new Date(ms).toLocaleString() : '';
}

function errorText(e) {
  return String(e && e.message ? e.message : e);
}

function query(name) {
  return new URLSearchParams(location.search).get(name);
}
