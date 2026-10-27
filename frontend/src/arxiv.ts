// The same id rules as src/arxiv.rs, so the home page can route straight to
// /abs/<id>. The server re-checks everything it is sent.

const NEW_STYLE = /^(\d{4})\.(\d{4,5})(?:v\d+)?$/
const OLD_STYLE = /^([a-z][a-z-]*(?:\.[A-Za-z]{2})?)\/(\d{7})(?:v\d+)?$/

export function parseId(input: string): string | null {
  let s = input.trim().replace(/^https?:\/\//i, '')
  s = s.replace(/^(www\.|export\.)?arxiv\.org\/(abs|pdf|html|format|src|e-print)\//i, '')
  s = s.replace(/^aero\.oops\.wtf\/paper-bro\/(abs|pdf|html)\//i, '')
  s = s.replace(/^arxiv:/i, '')
  s = s.split(/[?#]/)[0].replace(/\/+$/, '').replace(/\.pdf$/i, '')
  const m = NEW_STYLE.exec(s)
  if (m) {
    const fiveDigits = m[1] >= '1501'
    return fiveDigits === (m[2].length === 5) ? `${m[1]}.${m[2]}` : null
  }
  const o = OLD_STYLE.exec(s)
  if (o) {
    const [archive, cls] = o[1].split('.')
    return `${archive.toLowerCase()}${cls ? '.' + cls.toUpperCase() : ''}/${o[2]}`
  }
  return null
}
