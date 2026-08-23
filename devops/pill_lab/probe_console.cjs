// Probe: does `cmd /c start ... powershell -NoExit` open a visible console
// window that tails a log file? Counts powershell processes before/after and
// reports any spawn error.
const { spawn, execSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

const logPath = path.join(process.env.TEMP, 'pill_lab', 'probe.log');
fs.mkdirSync(path.dirname(logPath), { recursive: true });
fs.writeFileSync(logPath, 'probe line 1\nprobe line 2\n');

function countPowershell() {
  try {
    return Number(execSync('powershell -NoProfile -Command (Get-Process powershell).Count').toString().trim());
  } catch {
    return -1;
  }
}

const before = countPowershell();
const tail = `Get-Content -LiteralPath '${logPath}' -Wait -Tail 300`;
const cmdLine = `start "Pill Lab Probe" powershell.exe -NoExit -NoProfile -ExecutionPolicy Bypass -Command "${tail}"`;
console.log('cmdLine:', cmdLine);

const child = spawn('cmd.exe', ['/d', '/s', '/c', cmdLine], {
  windowsVerbatimArguments: true,
  windowsHide: false,
  stdio: 'ignore',
  detached: true,
});
child.on('error', (error) => console.log('spawn error:', error.message));
child.unref();

setTimeout(() => {
  const after = countPowershell();
  console.log('powershell before:', before, 'after:', after);
  process.exit(0);
}, 4000);
