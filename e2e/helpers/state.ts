import { readFileSync } from 'node:fs'

import { stateFile, type E2EState } from './env.js'

/** 读 globalSetup 写下的运行状态（hub 地址、管理员 token、daemon home……）。 */
export function readState(): E2EState {
  try {
    return JSON.parse(readFileSync(stateFile, 'utf8')) as E2EState
  } catch (err) {
    throw new Error(`cannot read ${stateFile}; did globalSetup run?`, { cause: err })
  }
}
