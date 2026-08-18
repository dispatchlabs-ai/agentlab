import { createReadStream, createWriteStream } from 'node:fs'
import {
  chmod,
  mkdir,
  readFile,
  rm,
  rmdir,
  stat,
  writeFile,
} from 'node:fs/promises'
import { createRequire } from 'node:module'
import { randomUUID } from 'node:crypto'
import { dirname, join } from 'node:path'
import { Readable } from 'node:stream'
import { pipeline } from 'node:stream/promises'
import { spawn } from 'node:child_process'
import { promisify } from 'node:util'
import { execFile as execFileCallback } from 'node:child_process'

const execFile = promisify(execFileCallback)
const MAX_EVENT_CHUNK = 24 * 1024
const FILE_UPLOAD_CHUNK_BYTES = 16 * 1024 * 1024
const HELPER_TIMEOUT_MS = 24 * 60 * 60 * 1000
let activeSandbox
let retainedSnapshots = []
let exiting = false

function protocol(type, fields = {}) {
  const line = JSON.stringify({ type, ...fields }) + '\n'
  if (!process.stdout.write(line)) {
    // Backpressure on an SSH pipe is uncommon, but the writable stream will
    // still buffer bounded event chunks until the local client catches up.
  }
}

function stage(message) {
  protocol('stage', { message })
}

function commandOutput(type, text) {
  const bytes = Buffer.from(text, 'utf8')
  for (let offset = 0; offset < bytes.length; offset += MAX_EVENT_CHUNK) {
    protocol(type, {
      data: bytes.subarray(offset, offset + MAX_EVENT_CHUNK).toString('base64'),
    })
  }
}

function shellQuote(value) {
  if (value === '') return "''"
  if (/^[A-Za-z0-9_@%+=:,./-]+$/.test(value)) return value
  return "'" + value.replaceAll("'", "'\"'\"'") + "'"
}

function validateAbsolutePath(value, label) {
  if (typeof value !== 'string' || !value.startsWith('/') || !/^[A-Za-z0-9_./-]+$/.test(value)) {
    throw new Error(`${label} is not a safe absolute path`)
  }
  const parts = value.slice(1).split('/')
  if (value === '/' || parts.some((part) => part === '' || part === '.' || part === '..')) {
    throw new Error(`${label} is not a canonical non-root path`)
  }
  return value
}

function validateRunId(value) {
  if (
    typeof value !== 'string' ||
    !/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(value)
  ) {
    throw new Error('AgentLab run ID is invalid')
  }
  return value
}

function validateStagingOutput(value, staging, label) {
  const output = validateAbsolutePath(value, label)
  if (!output.startsWith(`${staging}/`)) {
    throw new Error(`${label} is outside the private staging directory`)
  }
  return output
}

function validateBuildId(value) {
  if (typeof value !== 'string' || !/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(value)) {
    throw new Error('E2B build ID is invalid')
  }
  return value
}

function validateEnvironment(value) {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    throw new Error('E2B runtime environment must be an object')
  }
  const entries = Object.entries(value)
  if (entries.length > 128) throw new Error('E2B runtime environment exceeds 128 entries')
  let totalBytes = 0
  for (const [name, item] of entries) {
    if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(name) || typeof item !== 'string') {
      throw new Error('E2B runtime environment contains an invalid entry')
    }
    if (item.includes('\0')) throw new Error('E2B runtime environment contains a NUL byte')
    totalBytes += Buffer.byteLength(name) + Buffer.byteLength(item)
  }
  if (totalBytes > 256 * 1024) throw new Error('E2B runtime environment exceeds 256 KiB')
  return Object.fromEntries(entries.sort(([left], [right]) => left.localeCompare(right)))
}

function parseEnvironment(text) {
  for (const rawLine of text.split(/\r?\n/)) {
    const line = rawLine.trim()
    if (!line || line.startsWith('#')) continue
    const normalized = line.startsWith('export ') ? line.slice(7) : line
    const separator = normalized.indexOf('=')
    if (separator < 1) continue
    const key = normalized.slice(0, separator).trim()
    let value = normalized.slice(separator + 1).trim()
    if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(key)) continue
    if (
      value.length >= 2 &&
      ((value.startsWith("'") && value.endsWith("'")) ||
        (value.startsWith('"') && value.endsWith('"')))
    ) {
      value = value.slice(1, -1)
    }
    if (process.env[key] === undefined) process.env[key] = value
  }
}

async function readRequest() {
  const chunks = []
  let size = 0
  for await (const chunk of process.stdin) {
    size += chunk.length
    if (size > 16 * 1024 * 1024) throw new Error('helper request exceeds 16 MiB')
    chunks.push(chunk)
  }
  return JSON.parse(Buffer.concat(chunks).toString('utf8'))
}

async function loadSdk(request) {
  const sdkDirectory = validateAbsolutePath(request.sdk_directory, 'sdk_directory')
  parseEnvironment(await readFile(join(sdkDirectory, '.env.local'), 'utf8'))
  const require = createRequire(join(sdkDirectory, 'package.json'))
  const sdk = require('e2b')
  const packageRecord = JSON.parse(
    await readFile(join(sdkDirectory, 'node_modules/e2b/package.json'), 'utf8'),
  )
  return { ...sdk, sdkVersion: packageRecord.version }
}

function requestedTag(reference) {
  const finalSlash = reference.lastIndexOf('/')
  const colon = reference.lastIndexOf(':')
  return colon > finalSlash ? reference.slice(colon + 1) : 'default'
}

async function resolveTemplateBuild(Template, reference) {
  const tags = await Template.getTags(reference)
  const tag = requestedTag(reference)
  const selected = tags.find((entry) => entry.tag === tag)
  if (!selected) {
    throw new Error(`E2B template ${JSON.stringify(reference)} has no ${JSON.stringify(tag)} tag`)
  }
  return selected.buildId
}

async function resolveSnapshot(Template, snapshot) {
  const buildId = await resolveTemplateBuild(Template, snapshot.snapshotId)
  return {
    snapshot_id: snapshot.snapshotId,
    build_id: buildId,
    names: [...(snapshot.names ?? [])].sort(),
  }
}

async function flushFilesystemAndSnapshot({
  sandbox,
  Sandbox,
  Template,
  name,
  timeoutMs,
}) {
  const sandboxId = sandbox.sandboxId

  // A normal E2B snapshot captures Firecracker memory as well as the block
  // device. Writes that still live in the VM's block overlay are therefore
  // restored correctly, but they are not present in the directly mountable
  // rootfs artifact. AgentLab needs a mountable, immutable filesystem for
  // complete diff evidence, so first persist a filesystem-only checkpoint.
  await sandbox.pause({ keepMemory: false, requestTimeoutMs: 10 * 60 * 1000 })
  const resumed = await Sandbox.connect(sandboxId, {
    timeoutMs,
    requestTimeoutMs: 10 * 60 * 1000,
  })
  activeSandbox = resumed

  // Naming the resumed state gives AgentLab a durable provider reference for
  // later resume/fork work. Its rootfs is based on the filesystem-only
  // checkpoint above, so mounting this build observes the exact boundary that
  // AgentLab records as evidence.
  const raw = await resumed.createSnapshot({
    name,
    requestTimeoutMs: 10 * 60 * 1000,
  })
  // Track the provider reference before any subsequent lookup can fail so the
  // top-level cleanup handler can always delete a partially resolved snapshot.
  retainedSnapshots.push(raw.snapshotId)
  return {
    sandbox: resumed,
    snapshot: await resolveSnapshot(Template, raw),
  }
}

async function uploadChunk(sandbox, hostPath, guestPath, start, end) {
  const stream = Readable.toWeb(createReadStream(hostPath, { start, end }))
  await sandbox.files.write(guestPath, stream, {
    user: 'root',
    useOctetStream: true,
    requestTimeoutMs: 10 * 60 * 1000,
    signal: AbortSignal.timeout(10 * 60 * 1000),
  })
}

async function uploadFile(sandbox, hostPath, guestPath) {
  const target = validateAbsolutePath(guestPath, 'guest upload path')
  const source = await stat(hostPath)
  if (!source.isFile()) throw new Error('E2B upload source is not a regular file')

  if (source.size === 0) {
    await sandbox.files.write(target, new Uint8Array(), { user: 'root' })
    return
  }

  if (source.size <= FILE_UPLOAD_CHUNK_BYTES) {
    await uploadChunk(sandbox, hostPath, target, 0, source.size - 1)
    return
  }

  // E2B's file endpoint is proxied separately from the control API. Large
  // streamed requests can be terminated by an intermediary even though the
  // sandbox has enough disk. Keep every request bounded and verify each part
  // before appending it to the private destination inside the microVM.
  const part = `/tmp/.agentlab-upload-${randomUUID()}.part`
  await sandboxCommand(
    sandbox,
    `/bin/sh -c ${shellQuote(`umask 077; : > ${shellQuote(target)}`)}`,
    { timeoutMs: 30_000 },
  )
  try {
    for (let start = 0; start < source.size; start += FILE_UPLOAD_CHUNK_BYTES) {
      const end = Math.min(source.size, start + FILE_UPLOAD_CHUNK_BYTES) - 1
      const expected = end - start + 1
      await uploadChunk(sandbox, hostPath, part, start, end)
      const append = [
        'set -eu',
        `part=${shellQuote(part)}`,
        `target=${shellQuote(target)}`,
        `expected=${expected}`,
        'actual=$(stat -c %s -- "$part")',
        'test "$actual" -eq "$expected"',
        'cat -- "$part" >> "$target"',
        'rm -f -- "$part"',
      ].join('; ')
      await sandboxCommand(sandbox, `/bin/sh -c ${shellQuote(append)}`, {
        timeoutMs: 2 * 60 * 1000,
      })
    }
    const verify = [
      'set -eu',
      `target=${shellQuote(target)}`,
      `expected=${source.size}`,
      'actual=$(stat -c %s -- "$target")',
      'test "$actual" -eq "$expected"',
    ].join('; ')
    await sandboxCommand(sandbox, `/bin/sh -c ${shellQuote(verify)}`, {
      timeoutMs: 30_000,
    })
  } catch (error) {
    const cleanup = `rm -f -- ${shellQuote(part)} ${shellQuote(target)}`
    try {
      await sandboxCommand(sandbox, `/bin/sh -c ${shellQuote(cleanup)}`, {
        timeoutMs: 30_000,
      })
    } catch {}
    throw error
  }
}

async function downloadFile(sandbox, guestPath, hostPath) {
  await mkdir(dirname(hostPath), { recursive: true, mode: 0o700 })
  const stream = await sandbox.files.read(guestPath, {
    user: 'root',
    format: 'stream',
    requestTimeoutMs: 10 * 60 * 1000,
    streamIdleTimeoutMs: 10 * 60 * 1000,
  })
  await pipeline(Readable.fromWeb(stream), createWriteStream(hostPath, { mode: 0o600 }))
  await chmod(hostPath, 0o600)
}

async function sandboxCommand(sandbox, command, options = {}) {
  try {
    return await sandbox.commands.run(command, {
      user: options.user ?? 'root',
      cwd: options.cwd,
      timeoutMs: options.timeoutMs ?? 10 * 60 * 1000,
      requestTimeoutMs: options.requestTimeoutMs ?? 10 * 60 * 1000,
      onStdout: options.onStdout,
      onStderr: options.onStderr,
    })
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error)
    const stderr = typeof error?.stderr === 'string' ? error.stderr.trim() : ''
    const diagnostic = stderr && !message.includes(stderr) ? `${message}: ${stderr}` : message
    throw new Error(diagnostic.slice(0, 16 * 1024), { cause: error })
  }
}

const RUNNER = String.raw`#!/usr/bin/env python3
import json
import os
import selectors
import signal
import subprocess
import sys
import time

RUNTIME = "/run/agentlab-runtime"
with open(RUNTIME + "/command.json", "r", encoding="utf-8") as source:
    request = json.load(source)

limit = int(request["output_limit"])
deadline = time.monotonic() + int(request["timeout_seconds"])
stdout_path = RUNTIME + "/stdout.bin"
stderr_path = RUNTIME + "/stderr.bin"
process = subprocess.Popen(
    request["argv"],
    cwd=request["cwd"],
    env={**os.environ, **request["environment"]},
    stdin=subprocess.DEVNULL,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    start_new_session=True,
)
selector = selectors.DefaultSelector()
selector.register(process.stdout, selectors.EVENT_READ, (sys.stdout.buffer, stdout_path, "stdout"))
selector.register(process.stderr, selectors.EVENT_READ, (sys.stderr.buffer, stderr_path, "stderr"))
files = {
    stdout_path: open(stdout_path, "wb", buffering=0),
    stderr_path: open(stderr_path, "wb", buffering=0),
}
total = {"stdout": 0, "stderr": 0}
retained = {"stdout": 0, "stderr": 0}
timed_out = False
group_terminated = False

try:
    while selector.get_map():
        if process.poll() is None and time.monotonic() >= deadline:
            timed_out = True
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        if process.poll() is not None and not group_terminated:
            group_terminated = True
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        for key, _ in selector.select(0.25):
            source = key.fileobj
            target, path, name = key.data
            chunk = os.read(source.fileno(), 65536)
            if not chunk:
                selector.unregister(source)
                continue
            total[name] += len(chunk)
            keep = min(len(chunk), max(0, limit - retained[name]))
            if keep:
                portion = chunk[:keep]
                files[path].write(portion)
                target.write(portion)
                target.flush()
                retained[name] += keep
    direct_code = process.wait()
finally:
    for destination in files.values():
        destination.close()

exit_code = 124 if timed_out else direct_code
metadata = {
    "exit_code": exit_code,
    "timed_out": timed_out,
    "stdout_total_bytes": total["stdout"],
    "stderr_total_bytes": total["stderr"],
    "stdout_retained_bytes": retained["stdout"],
    "stderr_retained_bytes": retained["stderr"],
    "stdout_truncated": total["stdout"] > retained["stdout"],
    "stderr_truncated": total["stderr"] > retained["stderr"],
}
with open(RUNTIME + "/metadata.json.incoming", "w", encoding="utf-8") as destination:
    json.dump(metadata, destination, separators=(",", ":"))
    destination.flush()
    os.fsync(destination.fileno())
os.replace(RUNTIME + "/metadata.json.incoming", RUNTIME + "/metadata.json")
`

async function cleanupRuntime(sandbox, home, piAuth) {
  const authTarget = `${home}/.pi/agent/auth.json`
  const command = ['set -eu']
  if (piAuth) {
    // AgentLab proved this path absent before creating its lease. Remove it
    // unconditionally in case Pi atomically replaced the symlink while
    // refreshing OAuth, rather than retaining that replacement in a snapshot.
    command.push(`rm -f -- ${shellQuote(authTarget)}`)
  }
  command.push('rm -rf -- /run/agentlab-secrets /run/agentlab-runtime')
  await sandboxCommand(sandbox, `/bin/sh -c ${shellQuote(command.join('; '))}`, {
    timeoutMs: 30_000,
  })
}

async function cleanupActive() {
  const sandbox = activeSandbox
  activeSandbox = undefined
  if (sandbox) {
    try {
      await sandbox.kill({ requestTimeoutMs: 30_000 })
    } catch {}
  }
  const snapshots = retainedSnapshots
  retainedSnapshots = []
  if (globalThis.agentlabSandboxClass) {
    for (const snapshot of snapshots) {
      try {
        await globalThis.agentlabSandboxClass.deleteSnapshot(snapshot, { requestTimeoutMs: 30_000 })
      } catch {}
    }
  }
}

async function terminateFromSignal(code) {
  if (exiting) return
  exiting = true
  await cleanupActive()
  process.exit(code)
}

for (const [signal, code] of [['SIGINT', 130], ['SIGTERM', 143], ['SIGHUP', 129]]) {
  process.on(signal, () => void terminateFromSignal(code))
}
process.stdout.on('error', () => void terminateFromSignal(1))

async function actionProbe(request, sdk) {
  const buildId = await resolveTemplateBuild(sdk.Template, request.template)
  if (request.expected_template_build && buildId !== request.expected_template_build) {
    throw new Error(
      `E2B template tag resolved to build ${buildId}, expected ${request.expected_template_build}`,
    )
  }
  protocol('result', {
    result: {
      sdk_version: sdk.sdkVersion,
      template: request.template,
      template_build_id: buildId,
      isolation: 'firecracker',
    },
  })
}

async function actionRun(request, sdk) {
  const { Sandbox, Template } = sdk
  globalThis.agentlabSandboxClass = Sandbox
  const runId = validateRunId(request.run_id)
  const remoteRoot = validateAbsolutePath(request.remote_root, 'remote_root')
  const staging = validateAbsolutePath(request.staging, 'staging')
  if (staging !== join(remoteRoot, 'staging', runId)) {
    throw new Error('run staging does not match the configured private run directory')
  }
  const templateBuild = await resolveTemplateBuild(Template, request.template)
  if (request.expected_template_build && templateBuild !== request.expected_template_build) {
    throw new Error(
      `E2B template tag resolved to build ${templateBuild}, expected ${request.expected_template_build}`,
    )
  }
  const workspacePath = validateAbsolutePath(request.workspace_guest_path, 'workspace_guest_path')
  const environment = validateEnvironment(request.environment ?? {})
  const sandboxTimeoutMs = Math.min(
    HELPER_TIMEOUT_MS,
    Math.max(5 * 60 * 1000, Number(request.sandbox_timeout_ms ?? HELPER_TIMEOUT_MS)),
  )

  stage(`Creating Firecracker sandbox from ${request.template}`)
  let sandbox = await Sandbox.create(request.template, {
    timeoutMs: sandboxTimeoutMs,
    envs: environment,
    allowInternetAccess: request.network === 'bridge',
    metadata: {
      agentlab: 'true',
      agentlab_run_id: runId,
      agentlab_profile: request.profile,
    },
    lifecycle: { onTimeout: 'kill', autoResume: false },
  })
  activeSandbox = sandbox
  const templateBuildAfterCreate = await resolveTemplateBuild(Template, request.template)
  if (templateBuildAfterCreate !== templateBuild) {
    throw new Error(
      `E2B template tag changed during sandbox creation: ${templateBuild} became ${templateBuildAfterCreate}`,
    )
  }
  const sandboxInfo = await sandbox.getInfo()

  stage('Uploading private workspace to the sandbox')
  await uploadFile(sandbox, join(staging, 'workspace.tar'), '/tmp/agentlab-workspace.tar')
  await sandboxCommand(
    sandbox,
    [
      'set -eu',
      `workspace=${shellQuote(workspacePath)}`,
      'mkdir -p -- "$workspace"',
      'tar -xf /tmp/agentlab-workspace.tar -C "$workspace"',
      'rm -f -- /tmp/agentlab-workspace.tar',
    ].join('; '),
  )

  const architectureResult = await sandboxCommand(sandbox, 'uname -m', { timeoutMs: 30_000 })
  const architecture = architectureResult.stdout.trim()
  const homeResult = await sandbox.commands.run('printf %s "$HOME"', { timeoutMs: 30_000 })
  const commandHome = validateAbsolutePath(
    environment.HOME ?? homeResult.stdout.trim(),
    'sandbox command HOME',
  )

  stage('Flushing and retaining immutable base filesystem')
  const baseBoundary = await flushFilesystemAndSnapshot({
    sandbox,
    Sandbox,
    Template,
    name: `${request.snapshot_prefix}-base`,
    timeoutMs: sandboxTimeoutMs,
  })
  sandbox = baseBoundary.sandbox
  const baseSnapshot = baseBoundary.snapshot

  const secretArchive = join(staging, 'secrets.tar')
  if (request.secret_injections.length > 0) {
    stage('Injecting command-scoped credentials into runtime memory')
    await uploadFile(sandbox, secretArchive, '/tmp/agentlab-secrets.tar')
    await sandboxCommand(
      sandbox,
      [
        'set -eu',
        'umask 077',
        'rm -rf -- /run/agentlab-secrets',
        'mkdir -p /run/agentlab-secrets',
        'tar -xf /tmp/agentlab-secrets.tar -C /run/agentlab-secrets',
        'rm -f -- /tmp/agentlab-secrets.tar',
        'chmod 700 /run/agentlab-secrets',
        'find /run/agentlab-secrets -type f -exec chmod 600 {} +',
      ].join('; '),
    )
    await rm(secretArchive, { force: true })
    if (request.pi_auth) {
      const target = `${commandHome}/.pi/agent/auth.json`
      await sandboxCommand(
        sandbox,
        [
          'set -eu',
          `target=${shellQuote(target)}`,
          `parent=${shellQuote(dirname(target))}`,
          'if [ -e "$target" ] || [ -L "$target" ]; then echo "Pi authentication target already exists" >&2; exit 15; fi',
          'mkdir -p -- "$parent"',
          'ln -s /run/agentlab-secrets/pi-auth.json "$target"',
        ].join('; '),
      )
    }
  }

  stage('Running command in the Firecracker sandbox')
  await sandboxCommand(sandbox, 'rm -rf -- /run/agentlab-runtime; install -d -m 700 /run/agentlab-runtime')
  await sandbox.files.write('/run/agentlab-runtime/runner.py', RUNNER, { user: 'root' })
  await sandbox.files.write(
    '/run/agentlab-runtime/command.json',
    JSON.stringify({
      argv: request.command,
      cwd: workspacePath,
      environment,
      output_limit: request.output_limit,
      timeout_seconds: request.command_timeout_seconds,
    }),
    { user: 'root' },
  )
  await sandboxCommand(sandbox, 'chmod 700 /run/agentlab-runtime/runner.py; chmod 600 /run/agentlab-runtime/command.json')
  await sandboxCommand(sandbox, 'python3 /run/agentlab-runtime/runner.py', {
    user: undefined,
    cwd: workspacePath,
    timeoutMs: (request.command_timeout_seconds + 60) * 1000,
    requestTimeoutMs: (request.command_timeout_seconds + 120) * 1000,
    onStdout: (data) => commandOutput('command_stdout', data),
    onStderr: (data) => commandOutput('command_stderr', data),
  })

  await downloadFile(sandbox, '/run/agentlab-runtime/stdout.bin', join(staging, 'stdout.bin'))
  await downloadFile(sandbox, '/run/agentlab-runtime/stderr.bin', join(staging, 'stderr.bin'))
  const commandMetadata = JSON.parse(
    await sandbox.files.read('/run/agentlab-runtime/metadata.json', { user: 'root' }),
  )

  stage('Revoking runtime credentials')
  await cleanupRuntime(sandbox, commandHome, request.pi_auth)

  stage('Flushing and retaining immutable result filesystem')
  const resultBoundary = await flushFilesystemAndSnapshot({
    sandbox,
    Sandbox,
    Template,
    name: `${request.snapshot_prefix}-result`,
    timeoutMs: sandboxTimeoutMs,
  })
  sandbox = resultBoundary.sandbox
  const resultSnapshot = resultBoundary.snapshot
  const finalInfo = await sandbox.getInfo()

  await sandbox.kill({ requestTimeoutMs: 30_000 })
  activeSandbox = undefined
  // Successful runs deliberately retain both immutable snapshots.
  retainedSnapshots = []
  protocol('result', {
    result: {
      sdk_version: sdk.sdkVersion,
      template: request.template,
      template_build_id: templateBuild,
      sandbox_id: sandbox.sandboxId,
      architecture,
      base_snapshot: baseSnapshot,
      result_snapshot: resultSnapshot,
      command: commandMetadata,
      sandbox_info: sandboxInfo,
      final_sandbox_info: finalInfo,
    },
  })
}

async function waitForMount(mountPath, child, stderrChunks) {
  const deadline = Date.now() + 60_000
  while (Date.now() < deadline) {
    if (child.exitCode !== null) {
      throw new Error(
        `E2B rootfs mount exited early: ${Buffer.concat(stderrChunks).toString('utf8').slice(-4096)}`,
      )
    }
    try {
      await execFile('mountpoint', ['-q', mountPath], { timeout: 5_000 })
      return
    } catch {}
    await new Promise((resolve) => setTimeout(resolve, 100))
  }
  throw new Error('timed out waiting for E2B rootfs mount')
}

async function waitForChildExit(child, timeoutMs) {
  if (child.exitCode !== null) return true
  return await new Promise((resolve) => {
    let settled = false
    const finish = (exited) => {
      if (settled) return
      settled = true
      clearTimeout(timer)
      child.off('exit', onExit)
      resolve(exited)
    }
    const onExit = () => finish(true)
    child.once('exit', onExit)
    const timer = setTimeout(() => finish(false), timeoutMs)
    // The process can exit between the first check and listener registration.
    if (child.exitCode !== null) finish(true)
  })
}

async function stopMount(child, mountPath) {
  if (child.exitCode === null) {
    try {
      await execFile('sudo', ['-n', 'kill', '-INT', String(child.pid)], { timeout: 5_000 })
    } catch {}
    let exited = await waitForChildExit(child, 30_000)
    if (!exited) {
      try {
        await execFile('sudo', ['-n', 'umount', mountPath], { timeout: 10_000 })
      } catch {}
      try {
        await execFile('sudo', ['-n', 'kill', '-TERM', String(child.pid)], { timeout: 5_000 })
      } catch {}
      exited = await waitForChildExit(child, 10_000)
      if (!exited) {
        try {
          await execFile('sudo', ['-n', 'kill', '-KILL', String(child.pid)], { timeout: 5_000 })
        } catch {}
        await waitForChildExit(child, 10_000)
      }
    }
  }
  try {
    // Never recurse here: if unmounting failed, a recursive delete could walk
    // the mounted immutable rootfs. rmdir succeeds only for an unmounted,
    // empty mountpoint and otherwise leaves it for diagnosis.
    await rmdir(mountPath)
  } catch {}
}

async function withMountedBuild(request, callback) {
  const buildId = validateBuildId(request.build_id)
  const runId = validateRunId(request.run_id)
  const remoteRoot = validateAbsolutePath(request.remote_root, 'remote_root')
  const mountBinary = validateAbsolutePath(request.mount_binary, 'mount_binary')
  const mountPath = join(remoteRoot, 'mounts', `${runId}-${randomUUID()}`)
  await mkdir(mountPath, { recursive: false, mode: 0o700 })
  const stderrChunks = []
  const stdoutChunks = []
  const child = spawn(
    'sudo',
    [
      '-n',
      mountBinary,
      '-build',
      buildId,
      '-storage',
      join(remoteRoot, 'storage'),
      '-mount',
      mountPath,
    ],
    { stdio: ['ignore', 'pipe', 'pipe'] },
  )
  for (const [stream, chunks] of [[child.stdout, stdoutChunks], [child.stderr, stderrChunks]]) {
    stream.on('data', (chunk) => {
      chunks.push(chunk)
      while (chunks.reduce((sum, item) => sum + item.length, 0) > 1024 * 1024) chunks.shift()
    })
  }
  try {
    await waitForMount(mountPath, child, stderrChunks)
    await execFile('sudo', ['-n', 'mount', '-o', 'remount,ro', mountPath], {
      timeout: 30_000,
    })
    return await callback(mountPath)
  } finally {
    await stopMount(child, mountPath)
  }
}

async function actionSnapshotEvidence(request) {
  const scanner = validateAbsolutePath(request.scanner, 'scanner')
  const remoteRoot = validateAbsolutePath(request.remote_root, 'remote_root')
  const staging = validateAbsolutePath(request.staging, 'staging')
  const runId = validateRunId(request.run_id)
  if (staging !== join(remoteRoot, 'staging', runId)) {
    throw new Error('evidence staging does not match the configured private run directory')
  }
  const uid = process.getuid()
  const gid = process.getgid()
  await withMountedBuild(request, async (root) => {
    const scannerRequest = join(staging, `snapshot-request-${randomUUID()}.json`)
    const operation = request.operation
    const payload = { operation, root, uid, gid }
    if (operation === 'scan') {
      payload.output = validateStagingOutput(request.output, staging, 'output')
    } else if (operation === 'bundle') {
      payload.output = validateStagingOutput(request.output, staging, 'output')
      payload.paths = request.paths
    } else if (operation === 'captures') {
      payload.captures = request.captures.map((capture) => ({
        guest_path: capture.guest_path,
        output: validateStagingOutput(capture.output, staging, 'capture output'),
      }))
    } else {
      throw new Error(`unsupported snapshot evidence operation ${JSON.stringify(operation)}`)
    }
    await writeFile(scannerRequest, JSON.stringify(payload), { mode: 0o600, flag: 'wx' })
    try {
      await execFile('sudo', ['-n', scanner, scannerRequest], {
        timeout: HELPER_TIMEOUT_MS,
        maxBuffer: 4 * 1024 * 1024,
      })
    } finally {
      await rm(scannerRequest, { force: true })
    }
  })
  protocol('result', { result: { operation: request.operation } })
}

async function actionDelete(request, sdk) {
  const snapshots = []
  for (const snapshot of request.snapshots) {
    if (
      typeof snapshot.snapshot_id !== 'string' ||
      snapshot.snapshot_id.length === 0 ||
      snapshot.snapshot_id.length > 512 ||
      !/^[A-Za-z0-9._:/-]+$/.test(snapshot.snapshot_id)
    ) {
      throw new Error('refusing an invalid E2B snapshot reference')
    }
    const expectedBuild = validateBuildId(snapshot.build_id)
    const actualBuild = await resolveTemplateBuild(sdk.Template, snapshot.snapshot_id)
    if (actualBuild !== expectedBuild) {
      throw new Error(
        `E2B snapshot ${JSON.stringify(snapshot.snapshot_id)} resolves to build ${actualBuild}, expected ${expectedBuild}`,
      )
    }
    snapshots.push(snapshot.snapshot_id)
  }

  const deleted = []
  for (const snapshot of snapshots) {
    deleted.push({ snapshot, deleted: await sdk.Sandbox.deleteSnapshot(snapshot) })
  }
  protocol('result', { result: { deleted } })
}

async function actionCleanupStaging(request) {
  const remoteRoot = validateAbsolutePath(request.remote_root, 'remote_root')
  const staging = validateAbsolutePath(request.staging, 'staging')
  if (!staging.startsWith(`${remoteRoot}/staging/`)) {
    throw new Error('refusing to remove staging path outside the configured staging root')
  }
  await rm(staging, { recursive: true, force: true })
  protocol('result', { result: { removed: staging } })
}

async function main() {
  const request = await readRequest()
  if (request.action === 'snapshot_evidence') {
    await actionSnapshotEvidence(request)
    return
  }
  if (request.action === 'cleanup_staging') {
    await actionCleanupStaging(request)
    return
  }
  const sdk = await loadSdk(request)
  if (request.action === 'probe') await actionProbe(request, sdk)
  else if (request.action === 'run') await actionRun(request, sdk)
  else if (request.action === 'delete') await actionDelete(request, sdk)
  else throw new Error(`unsupported helper action ${JSON.stringify(request.action)}`)
}

try {
  await main()
} catch (error) {
  await cleanupActive()
  protocol('error', {
    message: error instanceof Error ? error.message : String(error),
  })
  process.exitCode = 1
}
