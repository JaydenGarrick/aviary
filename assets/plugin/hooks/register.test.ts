// `claude plugin test assets/plugin` — the kit runs hooks with no fs, so the
// status file and inbox are exercised live (see the README smoke); this
// checks the mod is a pure observer: every verdict and result passes
// through it untouched.
import { expect, test } from 'claude-code/testing'

test('tool.check hands the verdict beneath it back unchanged', async ($, on) => {
  on('tool.check', () => ({ decision: 'ask' as const, reason: 'the test asks' }))
  const verdict = await $.tool.check({ tool: 'Bash', input: { command: 'cargo test' } })
  expect(verdict.decision).toBe('ask')
  expect(verdict.reason).toBe('the test asks')
})
