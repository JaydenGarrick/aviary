// The aviary mod — runs INSIDE every bird and flock worker (the plugin rides
// `--plugin-dir ~/.config/aviary/plugin` into each session).
//
// Two jobs, both files, no sockets:
//   status/<sessionId>.json   what this session is doing — written on every
//                             change and heartbeated every 5 s, so the cockpit
//                             shows ⏸ needs you the moment a permission dialog
//                             opens, ✗ failed when a turn dies, and the
//                             context fill and cost.
//   inbox/<sessionId>/*.md    prompts the cockpit wants this session to run.
//                             Each is submitted with `$.prompt.submit`, which
//                             queues a turn of its own once the session is
//                             idle, then its name is acked in the status file.
//
// Aviary rewrites this file on every startup: edit
// `assets/plugin/hooks/register.ts` in the aviary repo, not this copy.
import { atom, read, update } from 'claude-code'
import type { EngineInterface, Register } from 'claude-code'

type State = 'working' | 'needs-input' | 'done' | 'failed' | 'compacting' | 'ended'

/** The last inbox file submitted — `$.state` survives a hot reload. */
const inboxAck = atom({ plugin: 'aviary', key: 'inboxAck' } as const, '')

const HEARTBEAT_MS = 5_000
const INBOX_MS = 1_000
/** Tools whose call IS a question to the person. */
const INTERACTIVE: Record<string, string> = {
  AskUserQuestion: 'question',
  ExitPlanMode: 'plan review',
}

/** What the session is doing, as the status file reports it. Module
 * variables start over on a hot reload; `session.start` fires again then. */
const current = {
  dir: '',
  cwd: '',
  startedAt: 0,
  state: 'done' as State,
  reason: '',
  detail: '',
  turnId: '',
  contextPercent: undefined as number | undefined,
  costUsd: undefined as number | undefined,
  draining: false,
}

function set(next: State, why = '', what = ''): void {
  current.state = next
  current.reason = why
  current.detail = what
}

/** An event of a subagent's loop never changes what the session itself is doing. */
function inSubagent(e: object): boolean {
  return 'agentId' in e && (e as { agentId?: string }).agentId !== undefined
}

/** `Bash · cargo test`: the tool and the head of its most telling argument. */
function describe(tool: string, input: unknown): string {
  const args = (input ?? {}) as Record<string, unknown>
  const head = (s: unknown): string =>
    typeof s === 'string' ? s.replace(/\s+/g, ' ').trim().slice(0, 48) : ''
  const what =
    head(args.command) || head(args.file_path) || head(args.path) || head(args.url) || head(args.description)
  return what === '' ? tool : `${tool} · ${what}`
}

/** Write the status file whole. A failed write never breaks the session's chain. */
async function write($: EngineInterface): Promise<void> {
  if (current.dir === '') return
  try {
    // Read the id every time: a /clear moves the session to a new one.
    const id = await $.session.id()
    const body = {
      v: 1,
      session_id: id,
      cwd: current.cwd,
      state: current.state,
      reason: current.reason === '' ? undefined : current.reason,
      detail: current.detail === '' ? undefined : current.detail,
      turn_id: current.turnId === '' ? undefined : current.turnId,
      context_percent: current.contextPercent,
      cost_usd: current.costUsd,
      inbox_ack: await read($, inboxAck),
      started_at: current.startedAt,
      updated_at: await $.clock.now(),
    }
    await $.fs.write(`${current.dir}/status/${id}.json`, JSON.stringify(body))
  } catch {
    // The cockpit reads the last good file until the next write lands.
  }
}

/** Submit every inbox file after the ack, oldest first, acking each. */
async function drain($: EngineInterface): Promise<void> {
  if (current.draining || current.dir === '') return
  current.draining = true
  try {
    const id = await $.session.id()
    const folder = `${current.dir}/inbox/${id}`
    if (!(await $.fs.exists(folder))) return
    const ack = await read($, inboxAck)
    const names = (await $.fs.list(folder))
      .filter(f => f.kind === 'file' && f.name.endsWith('.md') && f.name > ack)
      .map(f => f.name)
      .sort()
    for (const name of names) {
      const text = await $.fs.read(`${folder}/${name}`)
      if (text.trim() !== '') {
        // Resolves once the prompt's own turn starts — after the running
        // turn, if any. `asUser` keeps the wording bare, as if typed.
        await $.prompt.submit({ text, asUser: true })
      }
      await update($, inboxAck, () => name)
      await write($)
    }
  } catch {
    // Listing or reading failed: the next period tries again.
  } finally {
    current.draining = false
  }
}

export const register: Register = on => {
  on('session.start', async ($, e, next) => {
    const home = (await $.env.get('HOME')) ?? ''
    current.dir = (await $.env.get('AVIARY_CONFIG_DIR')) ?? `${home}/.config/aviary`
    current.cwd = e.cwd
    current.startedAt = await $.clock.now()
    set('done')
    await write($)
    $.clock.every(HEARTBEAT_MS, () => void write($))
    $.clock.every(INBOX_MS, () => void drain($))
    return next(e)
  })

  on('turn.start', async ($, e, next) => {
    if (!inSubagent(e)) {
      current.turnId = e.turnId
      set('working')
      await write($)
    }
    return next(e)
  })

  on('turn.complete', async ($, e, next) => {
    if (!inSubagent(e)) {
      if (e.reason === 'error') set('failed', 'error')
      else if (e.reason === 'aborted') set('done', 'interrupted')
      else set('done')
      await write($)
    }
    return next(e)
  })

  // The verdict is read here, before any dialog: `ask` means one opens now.
  // A subagent's ask opens the same dialog in the same session — it blocks
  // the person just as much, so it is NOT filtered like the loop events are.
  // A `$.tool.check` query (no `tool_use_id`) opens no dialog and is ignored.
  on('tool.check', async ($, e, next) => {
    const verdict = await next(e)
    if (verdict.decision === 'ask' && e.tool_use_id !== undefined) {
      set('needs-input', 'permission', describe(e.tool, e.input))
      await write($)
    }
    return verdict
  }).catch(($, e, next) => next(e))

  on('tool.call', async ($, e, next) => {
    const asks = inSubagent(e) ? undefined : INTERACTIVE[String(e.tool)]
    if (asks !== undefined) {
      set('needs-input', asks, String(e.tool))
      await write($)
    }
    const ran = await next(e)
    // The dialog (permission or question) is answered once the call returns.
    // A subagent's returning call clears only a permission wait (the dialog
    // it may have raised), never the main loop's question or plan review.
    if (current.state === 'needs-input' && (!inSubagent(e) || current.reason === 'permission')) {
      set('working')
      await write($)
    }
    return ran
  }).catch(($, e, next) => next(e))

  on('session.measure', async ($, e, next) => {
    current.contextPercent = e.context.percent
    current.costUsd = e.cost?.usd
    await write($)
    return next(e)
  })

  on('session.compact', async ($, e, next) => {
    if (inSubagent(e)) return next(e)
    const before = current.state
    set('compacting')
    await write($)
    const result = await next(e)
    set(before)
    await write($)
    return result
  }).catch(($, e, next) => next(e))

  on('session.end', async ($, e, next) => {
    set('ended', e.reason)
    await write($)
    // A /clear goes on under a new id: the next heartbeat reports it quiet.
    if (e.reason === 'clear') set('done')
    return next(e)
  })
}
