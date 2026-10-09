// The aviary mod's `$.state` contract: what it keeps in the session's state
// (`claude plugin validate` holds every `$.state` key the module names to it).
export type AviaryInboxAck = string

declare module 'claude-code' {
  interface PluginState {
    aviary: {
      /** The last inbox file submitted — survives a hot reload, so a reload never re-submits it. */
      inboxAck: string
    }
  }
}
