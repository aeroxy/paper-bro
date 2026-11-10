import { Fragment, useCallback, useEffect, useRef, useState } from 'react'
import { Link, useLocation, useSearchParams } from 'react-router-dom'
import { ApiError, build, eventsUrl, getPaper, report, type Paper, type Progress } from './api'
import { parseId } from './arxiv'
import { DONATE } from './Shell'

const STEPS: { stage: string; label: string }[] = [
  { stage: 'queued', label: 'In the queue' },
  { stage: 'ingest', label: 'Reading the paper' },
  { stage: 'outline', label: 'Planning the lessons' },
  { stage: 'lessons', label: 'Writing lessons and labs' },
  { stage: 'qa', label: 'Testing every lab in a browser' },
  { stage: 'review', label: 'Checking it against the paper' },
  { stage: 'repair', label: 'Fixing what the checks found' },
  { stage: 'publish', label: 'Publishing' },
]

// Every new course is posted there (src/telegram.rs).
const TELEGRAM = 'https://t.me/arxiv_courses'

export function PaperPage() {
  const { pathname } = useLocation()
  const [params] = useSearchParams()
  const raw = decodeURIComponent(pathname.replace(/^\/(abs|pdf|html)\//, ''))
  const id = parseId(raw)
  const [paper, setPaper] = useState<Paper | null>(null)
  const [error, setError] = useState<string>('')
  const [busy, setBusy] = useState(false)
  const wantsReport = params.has('report')

  const load = useCallback(() => {
    if (!id) return
    getPaper(id).then(
      (p) => {
        setPaper(p)
        if (p.status === 'ready' && p.url && !wantsReport) window.location.replace(p.url)
      },
      (e: ApiError) => setError(e.message),
    )
  }, [id, wantsReport])

  useEffect(load, [load])

  // A failed build can be tried again an hour after it stopped (src/web.rs).
  // Ask again then, so the button appears without a reload.
  const retryAt = paper && !paper.retryable ? paper.retry_at : null
  useEffect(() => {
    if (!retryAt) return
    const t = setTimeout(load, Math.max(new Date(retryAt).getTime() - Date.now() + 1000, 0))
    return () => clearTimeout(t)
  }, [retryAt, load])

  if (!id) return <Notice title="That isn't an arXiv paper id." body={`“${raw}” doesn't look like one. Try 1706.03762 or an arxiv.org link.`} />
  if (error) return <Notice title="Something's not right." body={error} />
  if (!paper) return <p className="pt-16 text-[14px] text-fg-3">Looking up arXiv:{id}…</p>

  const start = async () => {
    setBusy(true)
    setError('')
    try {
      setPaper(await build(paper.id))
    } catch (e) {
      setError((e as ApiError).message)
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="grid gap-8 pt-12 lg:grid-cols-[1.1fr_.9fr]">
      <PaperCard paper={paper} />
      <div>
        {wantsReport && paper.status !== 'none' ? (
          <ReportForm id={paper.id} />
        ) : paper.status === 'none' ? (
          <div className="card">
            <div className="eyebrow">No course yet</div>
            <h2 className="mt-2 text-2xl font-extrabold tracking-tight">Nobody has asked for this one.</h2>
            <p className="mt-3 text-[14.5px] text-fg-2">
              paper-bro will read the paper and write 6 to 10 lessons with hands-on labs and a quiz, for curious beginners.
              It takes about 5 to 10 minutes, and then the course is here for everyone.
            </p>
            <button className="btn-accent mt-5" onClick={start} disabled={busy}>
              {busy ? 'Starting…' : 'Build the course →'}
            </button>
          </div>
        ) : paper.status === 'queued' || paper.status === 'running' ? (
          <Building paper={paper} onDone={load} />
        ) : paper.status === 'failed' ? (
          <div className="card">
            <div className="eyebrow text-danger-strong">Couldn't build this one</div>
            <p className="mt-3 text-[14.5px] text-fg-2">{paper.error}</p>
            {paper.retryable ? (
              <button className="btn-sec mt-5" onClick={start} disabled={busy}>
                Try again
              </button>
            ) : paper.retry_at ? (
              <p className="mt-3 text-[13px] text-fg-3">
                You can try again after{' '}
                {new Date(paper.retry_at).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' })}.
              </p>
            ) : null}
          </div>
        ) : paper.status === 'removed' ? (
          <div className="card">
            <div className="eyebrow">Removed</div>
            <p className="mt-3 text-[14.5px] text-fg-2">This paper's course was taken down.</p>
          </div>
        ) : (
          <div className="card">
            <div className="eyebrow">Ready</div>
            <a className="btn-accent mt-4" href={paper.url ?? '#'}>Open the course →</a>
          </div>
        )}
      </div>
    </div>
  )
}

// A progress message as text, except that the donation address the server
// names when it runs out of credit (src/pipeline.rs) becomes a link. Only
// that address: other text can come from a paper, and stays plain.
function Message({ text }: { text: string }) {
  return (
    <>
      {text.split(DONATE).map((part, i) => (
        <Fragment key={i}>
          {i > 0 && (
            <a className="text-action underline underline-offset-2 hover:text-action-700" href={DONATE} target="_blank" rel="noopener">
              {DONATE}
            </a>
          )}
          {part}
        </Fragment>
      ))}
    </>
  )
}

function PaperCard({ paper }: { paper: Paper }) {
  const year = paper.published ? new Date(paper.published).getFullYear() : null
  const authors = paper.authors.length > 6 ? `${paper.authors.slice(0, 6).join(', ')} and others` : paper.authors.join(', ')
  return (
    <div>
      <div className="eyebrow">
        arXiv:{paper.id}
        {year && <span className="mx-1.5 text-action">·</span>}
        {year}
        {paper.categories[0] && <span className="mx-1.5 text-action">·</span>}
        {paper.categories[0]}
      </div>
      <h1 className="mt-3 text-[clamp(28px,4vw,42px)] font-extrabold leading-[1.02] tracking-[-.04em]">{paper.title}</h1>
      <p className="mt-3 text-[14px] text-fg-2">{authors}</p>
      <p className="mt-5 max-w-2xl text-[14.5px] leading-relaxed text-fg-2">{paper.abstract}</p>
      <a className="mt-4 inline-block text-[14px] font-semibold text-action no-underline hover:text-action-700" href={paper.abs_url}>
        Read it on arXiv →
      </a>
    </div>
  )
}

function Building({ paper, onDone }: { paper: Paper; onDone: () => void }) {
  const [events, setEvents] = useState<Progress[]>([])
  const doneRef = useRef(onDone)
  doneRef.current = onDone

  useEffect(() => {
    const es = new EventSource(eventsUrl(paper.id))
    es.addEventListener('progress', (m) => {
      const ev: Progress = JSON.parse((m as MessageEvent).data)
      setEvents((xs) => (xs.some((x) => x.id === ev.id) ? xs : [...xs, ev]))
      if (ev.stage === 'ready' || ev.stage === 'failed') {
        es.close()
        doneRef.current()
      }
    })
    return () => es.close()
  }, [paper.id])

  const latest = [...events].reverse().find((e) => STEPS.some((s) => s.stage === e.stage))
  const current = Math.max(0, STEPS.findIndex((s) => s.stage === (latest?.stage ?? paper.stage ?? 'queued')))
  const visible = events.filter((e) => !['visitor-error', 'report'].includes(e.stage))
  // The server reports "credit" when the model provider is out of credit
  // (src/pipeline.rs). The course then waits, so the headline says that and
  // doesn't promise minutes. The latest event decides: the headline follows
  // the log as it happens, and lets go once the course moves again.
  const onHold = visible[visible.length - 1]?.stage === 'credit'

  return (
    <div className="card">
      <div className="eyebrow">Building the course</div>
      <h2 className="mt-2 text-2xl font-extrabold tracking-tight">
        {onHold
          ? 'Waiting for model credit.'
          : paper.status === 'queued' && paper.queue_position && paper.queue_position > 1
            ? `Number ${paper.queue_position} in the queue.`
            : 'This takes 5 to 10 minutes.'}
      </h2>
      <p className="mt-2 text-[13.5px] text-fg-3">
        You can leave and come back: the course will be here when it's done. To hear when it is, follow{' '}
        <a className="font-semibold text-action no-underline hover:text-action-700" href={TELEGRAM} target="_blank" rel="noopener">
          paper-bro on Telegram
        </a>
        , where every new course is posted.
      </p>
      <ol className="mt-5 grid gap-2">
        {STEPS.map((s, i) => (
          <li key={s.stage} className="flex items-center gap-3 text-[14px]">
            <span
              className={
                'grid h-5 w-5 flex-none place-items-center rounded-full border text-[10px] ' +
                (i < current
                  ? 'border-status-300 bg-status-soft text-status-700'
                  : i === current
                    ? 'animate-pulse border-action-300 bg-action-soft text-action-700'
                    : 'border-line text-fg-3')
              }
            >
              {i < current ? '✓' : i + 1}
            </span>
            <span className={i <= current ? 'text-fg' : 'text-fg-3'}>{s.label}</span>
          </li>
        ))}
      </ol>
      {visible.length > 0 && (
        <div className="mt-5 max-h-48 overflow-auto rounded-xl border border-line bg-sunken p-3 font-mono text-[12px] leading-relaxed text-fg-2">
          {visible.map((e) => (
            <div key={e.id}>
              <span className="text-fg-3">{new Date(e.at).toLocaleTimeString()}</span> <Message text={e.message} />
            </div>
          ))}
        </div>
      )}
    </div>
  )
}

function ReportForm({ id }: { id: string }) {
  const [reason, setReason] = useState('')
  const [state, setState] = useState<'idle' | 'sending' | 'sent'>('idle')
  const [error, setError] = useState('')
  if (state === 'sent')
    return (
      <div className="card">
        <div className="eyebrow">Thanks</div>
        <p className="mt-3 text-[14.5px] text-fg-2">Your report was sent. It's read by a person.</p>
      </div>
    )
  return (
    <form
      className="card"
      onSubmit={async (e) => {
        e.preventDefault()
        setState('sending')
        try {
          await report(id, reason)
          setState('sent')
        } catch (err) {
          setError((err as ApiError).message)
          setState('idle')
        }
      }}
    >
      <div className="eyebrow">Report a problem</div>
      <p className="mt-2 text-[14px] text-fg-2">
        Something wrong in this course: a mistake about the paper, a broken lab, or content that shouldn't be here? Tell us.
      </p>
      <textarea
        value={reason}
        onChange={(e) => setReason(e.target.value)}
        maxLength={1000}
        rows={5}
        className="mt-3 w-full rounded-xl border border-line bg-white p-3 text-[14px] outline-none focus:border-action"
        placeholder="What's wrong, and in which lesson?"
      />
      {error && <p className="mt-2 text-[13px] text-danger-strong">{error}</p>}
      <button className="btn-accent mt-3" disabled={state === 'sending' || !reason.trim()}>
        {state === 'sending' ? 'Sending…' : 'Send report'}
      </button>
    </form>
  )
}

function Notice({ title, body }: { title: string; body: string }) {
  return (
    <div className="max-w-xl pt-16">
      <h1 className="text-3xl font-extrabold tracking-tight">{title}</h1>
      <p className="mt-3 text-[15px] text-fg-2">{body}</p>
      <Link to="/" className="mt-5 inline-block text-[14px] font-semibold text-action no-underline">
        ← Back to paper-bro
      </Link>
    </div>
  )
}
