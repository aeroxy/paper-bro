import { useEffect, useState, type FormEvent } from 'react'
import { useNavigate } from 'react-router-dom'
import { recent, type CourseCard } from './api'
import { parseId } from './arxiv'

const EXAMPLES = [
  { id: '1706.03762', label: 'Attention Is All You Need' },
  { id: '1512.03385', label: 'ResNet' },
  { id: '2006.11239', label: 'Diffusion models' },
]

export function Home() {
  const navigate = useNavigate()
  const [input, setInput] = useState('')
  const [error, setError] = useState('')
  const [courses, setCourses] = useState<CourseCard[] | null>(null)

  useEffect(() => {
    recent().then(setCourses, () => setCourses([]))
  }, [])

  const go = (e: FormEvent) => {
    e.preventDefault()
    const id = parseId(input)
    if (!id) {
      setError("That doesn't look like an arXiv paper. Paste a link like arxiv.org/abs/1706.03762, or just the id.")
      return
    }
    navigate(`/abs/${id}`)
  }

  return (
    <>
      <section className="pb-12 pt-14 sm:pt-20">
        <div className="eyebrow">
          Free<span className="mx-1.5 text-action">·</span>No login<span className="mx-1.5 text-action">·</span>One course per paper
        </div>
        <h1 className="mt-4 max-w-3xl text-[clamp(36px,6vw,64px)] font-extrabold leading-[.95] tracking-[-.045em]">
          Any arXiv paper,
          <br />
          as a course you can play with.
        </h1>
        <p className="mt-5 max-w-xl text-[17px] text-fg-2">
          Paste a paper and paper-bro writes an interactive course about it: short lessons for curious beginners,
          each with a hands-on lab, then a quiz. If someone already made one, you go straight to it.
        </p>
        <form onSubmit={go} className="mt-8 flex max-w-2xl flex-col gap-2.5 sm:flex-row">
          <input
            value={input}
            onChange={(e) => {
              setInput(e.target.value)
              setError('')
            }}
            placeholder="arxiv.org/abs/1706.03762, or just 1706.03762"
            aria-label="arXiv link or id"
            className="h-12 w-full rounded-xl sm:flex-1 border border-line bg-white px-4 text-[15px] outline-none transition focus:border-action focus:shadow-[0_0_0_3px_rgba(216,90,48,.18)]"
          />
          <button className="btn-accent h-12 justify-center px-6 text-[14px]">Open the course →</button>
        </form>
        {error && <p className="mt-2.5 text-[13.5px] text-danger-strong">{error}</p>}
        <p className="mt-4 text-[13px] text-fg-3">
          Shortcut: on any paper page, change <span className="font-mono text-fg-2">arxiv.org</span> in the address to{' '}
          <span className="font-mono text-fg-2">aero.oops.wtf/paper-bro</span>.
        </p>
        <div className="mt-5 flex flex-wrap gap-2">
          {EXAMPLES.map((x) => (
            <button
              key={x.id}
              onClick={() => navigate(`/abs/${x.id}`)}
              className="rounded-full border border-line bg-white px-3 py-1 text-[12px] font-medium text-fg-2 hover:border-line-strong hover:text-fg"
            >
              {x.label}
            </button>
          ))}
        </div>
      </section>

      <section className="border-t border-line pt-10">
        <div className="eyebrow">Recently built</div>
        {courses === null ? (
          <p className="mt-4 text-[14px] text-fg-3">Loading…</p>
        ) : courses.length === 0 ? (
          <p className="mt-4 text-[14px] text-fg-3">No courses yet. Be the first: paste a paper above.</p>
        ) : (
          <div className="mt-5 grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
            {courses.map((c) => (
              <a
                key={c.id}
                href={c.url}
                className="group rounded-2xl border border-line bg-white p-5 no-underline transition hover:-translate-y-0.5 hover:shadow-card"
              >
                <div className="font-mono text-[11px] text-fg-3">arXiv:{c.id}</div>
                <div className="mt-1.5 text-[15px] font-bold leading-snug tracking-tight text-ink group-hover:text-action-700">
                  {c.title}
                </div>
              </a>
            ))}
          </div>
        )}
      </section>
    </>
  )
}
