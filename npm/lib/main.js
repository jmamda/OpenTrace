#!/usr/bin/env node
'use strict';

const { spawnSync } = require('child_process');
const path = require('path');
const os = require('os');

const PLATFORM_PACKAGES = {
  'linux-x64':    '@opentraceai/trace-linux-x64',
  'linux-arm64':  '@opentraceai/trace-linux-arm64',
  'darwin-x64':   '@opentraceai/trace-darwin-x64',
  'darwin-arm64': '@opentraceai/trace-darwin-arm64',
  'win32-x64':    '@opentraceai/trace-win32-x64',
};

function getBinaryPath() {
  const key = `${process.platform}-${os.arch()}`;
  const pkg = PLATFORM_PACKAGES[key];
  if (!pkg) {
    console.error(`@opentraceai/trace: unsupported platform ${key}`);
    console.error(`Supported: ${Object.keys(PLATFORM_PACKAGES).join(', ')}`);
    process.exit(1);
  }
  try {
    // require.resolve finds the platform package's package.json,
    // then we navigate to the binary alongside it.
    const pkgJson = require.resolve(`${pkg}/package.json`);
    const pkgDir  = path.dirname(pkgJson);
    const bin     = process.platform === 'win32'
      ? path.join(pkgDir, 'trace.exe')
      : path.join(pkgDir, 'bin', 'trace');
    return bin;
  } catch {
    console.error(`@opentraceai/trace: platform package '${pkg}' not installed.`);
    console.error(`Run: npm install ${pkg}`);
    process.exit(1);
  }
}

const result = spawnSync(getBinaryPath(), process.argv.slice(2), {
  stdio: 'inherit',
  cwd: process.cwd(),
});

process.exit(result.status ?? 1);
