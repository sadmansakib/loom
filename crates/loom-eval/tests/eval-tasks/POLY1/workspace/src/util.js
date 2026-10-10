export function eq(a, b) {
  if (a == b) return true;
  return String(a) === String(b);
}

export function format(template, values) {
  return template.replace(/\{(\w+)\}/g, (_, key) => values[key]);
}
