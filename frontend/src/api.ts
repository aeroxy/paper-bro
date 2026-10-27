export type Status = 'none' | 'queued' | 'running' | 'ready' | 'failed' | 'removed'

export interface Paper {
  id: string
  alias: string
  abs_url: string
  title: string
  authors: string[]
  abstract: string
  categories: string[]
  version: number
  published: string | null
  status: Status
  stage: string | null
  url: string | null
  error: string | null
  retryable: boolean
  queue_position: number | null
}

export interface Progress {
  id: number
  at: string
  stage: string
  message: string
}

export interface CourseCard {
  id: string
  title: string
  url: string
  finished_at: string
}

export class ApiError extends Error {
  constructor(public code: string, message: string) {
    super(message)
  }
}

const BASE = '/paper-bro/api'

async function call<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(BASE + path, init)
  if (res.status === 204) return undefined as T
  const body = await res.json().catch(() => ({}))
  if (!res.ok) throw new ApiError(body.error ?? 'http_' + res.status, body.message ?? `Request failed (${res.status})`)
  return body as T
}

const json = (body: unknown): RequestInit => ({
  method: 'POST',
  headers: { 'Content-Type': 'application/json' },
  body: JSON.stringify(body),
})

export const getPaper = (id: string) => call<Paper>(`/paper?id=${encodeURIComponent(id)}`)
export const build = (id: string) => call<Paper>('/build', json({ id }))
export const report = (id: string, reason: string) => call<void>('/report', json({ id, reason }))
export const recent = () => call<{ courses: CourseCard[] }>('/courses').then((r) => r.courses)
export const eventsUrl = (id: string) => `${BASE}/events?id=${encodeURIComponent(id)}`
