#!/usr/bin/env node
// Compiles a JavaScript guest (a directory with index.js and wit/) to a WIT
// component with componentize-qjs.
//
//   node build.mjs <guest-dir> <out.wasm>
//
// The guest's wit/deps/tauri-backend/backend.wit must exist; the Rust test
// harness writes it from `wasm_backend_core::WIT`.

import { mkdirSync, readFileSync, renameSync, writeFileSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { componentize } from 'componentize-qjs'

const [dir, out] = process.argv.slice(2).map((p) => resolve(p))
if (!dir || !out) {
  console.error('usage: node build.mjs <guest-dir> <out.wasm>')
  process.exit(2)
}

const entry = join(dir, 'index.js')
const { component } = await componentize({
  witPath: join(dir, 'wit'),
  jsSource: readFileSync(entry, 'utf8'),
  jsPath: entry,
  // Plain (non-async) component ABI.
  sync: true,
})

mkdirSync(dirname(out), { recursive: true })
writeFileSync(`${out}.tmp`, component)
renameSync(`${out}.tmp`, out)
console.log(`built ${out} (${(component.length / 1024).toFixed(0)} KiB)`)
