// Explicit owned Chromium textarea fixture. CDP only reads native paste events;
// expected text is never assigned to the DOM or sent as a keyboard payload.
import fs from 'node:fs';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import {spawn, execFileSync} from 'node:child_process';
import {once} from 'node:events';
import readline from 'node:readline';
import crypto from 'node:crypto';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const [backend, outputArgument] = process.argv.slice(2);
const iterations=Number(process.argv[4]||1);
if(![1,20].includes(iterations))throw Error('Only 1 or 20 explicit iterations supported');
const driverProfile=process.argv[5]==='--release'?'release':'debug';
if(process.argv[5]&&process.argv[5]!=='--release')throw Error('Unknown driver profile');
const driverBinary=path.join(root,'target',driverProfile,'examples/browser_paste');
if (!['wayland', 'x11'].includes(backend) || !outputArgument) throw Error('browser-paste-probe.mjs wayland|x11 NEW.json');
const output = path.resolve(outputArgument);
if (fs.existsSync(output)) throw Error('Evidence exists');
const expected = '语音输入粘贴验证 2026\nVoice input paste probe: /tmp/example.txt';
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
const command = (name, args, options={}) => execFileSync(name, args, {maxBuffer:8*1024*1024,...options});
const hypr = name => JSON.parse(command('hyprctl', ['-j',name]).toString());
if (hypr('locked').locked !== false) throw Error('Unlocked desktop required');
const saved = {};
let types;
try { types=command('wl-paste',['--list-types']).toString().trim().split('\n').filter(Boolean); }
catch(error) { if (!String(error.stderr).includes('Nothing is copied')) throw error; types=[]; }
if (types.length>32) throw Error('Clipboard MIME bound exceeded');
for (const mime of types) {
  saved[mime]=command('wl-paste',['--no-newline','--type',mime]);
  if (Object.values(saved).reduce((n,b)=>n+b.length,0)>8*1024*1024) throw Error('Clipboard byte bound exceeded');
}

async function connect(url) {
  const socket = new WebSocket(url);
  await new Promise((resolve,reject)=>{
    const timer=setTimeout(()=>{try{socket.close();}catch{}reject(Error('CDP connection timeout'));},3000);
    socket.addEventListener('open',()=>{clearTimeout(timer);resolve();},{once:true});
    socket.addEventListener('error',()=>{clearTimeout(timer);reject(Error('CDP connection error'));},{once:true});
  });
  let sequence=0;
  const pending=new Map();
  socket.addEventListener('message',event=>{
    const value=JSON.parse(event.data);
    const call=pending.get(value.id);
    if(call){pending.delete(value.id);clearTimeout(call.timer);value.error?call.reject(Error('CDP command failed')):call.resolve(value.result);}
  });
  return {socket, call(method,params={}){return new Promise((resolve,reject)=>{
    const id=++sequence;
    const timer=setTimeout(()=>{pending.delete(id);reject(Error('CDP timeout'));},3000);
    pending.set(id,{resolve,reject,timer});socket.send(JSON.stringify({id,method,params}));
  });}};
}

const temporary=fs.mkdtempSync(path.join(root,'build/owned-browser-'));
const profile=path.join(temporary,'profile');fs.mkdirSync(profile);
const html=path.join(temporary,'fixture.html');
fs.writeFileSync(html,'<!doctype html><meta charset="utf-8"><title>Voice Input Owned Browser Probe</title><h1>合成粘贴验证</h1><textarea id="probe" rows="8" autofocus></textarea><script>window.probe={input:false,paste:false};const field=document.getElementById("probe");field.addEventListener("input",event=>{window.probe.input ||= event.isTrusted;});field.addEventListener("paste",event=>{window.probe.paste ||= event.isTrusted;});</script>');
const report={scope:'Owned isolated Chromium textarea, fixed synthetic text through production Hyprland adapter; no microphone/provider/full V01/V11 acceptance',backend,iterations,driver_profile:driverProfile,clipboard_snapshot_mime_count:types.length,passed:false};
let browser,driver,browserCdp,pageCdp;
try {
  browser=spawn('chromium',[`--user-data-dir=${profile}`,`--ozone-platform=${backend}`,'--remote-debugging-port=0','--enable-automation','--no-first-run','--no-default-browser-check','--disable-background-networking','--disable-sync','--disable-extensions','--disable-component-update','--disable-breakpad',`--app=file://${html}`],{stdio:'ignore'});
  const activePort=path.join(profile,'DevToolsActivePort');
  const deadline=Date.now()+10000;
  while(!fs.existsSync(activePort)){if(Date.now()>deadline)throw Error('Owned browser startup timeout');await delay(50);}
  const [port,endpoint]=fs.readFileSync(activePort,'utf8').trim().split('\n');
  if(!/^\d+$/.test(port)||!/^\/devtools\/browser\/[\w-]+$/.test(endpoint))throw Error('Invalid owned CDP endpoint');
  browserCdp=await connect(`ws://127.0.0.1:${port}${endpoint}`);
  const processes=await browserCdp.call('SystemInfo.getProcessInfo');
  const pid=processes.processInfo.find(item=>item.type==='browser')?.id;
  if(!Number.isInteger(pid))throw Error('Owned browser PID missing');
  const argv=fs.readFileSync(`/proc/${pid}/cmdline`).toString().split('\0');
  const parsed=(await browserCdp.call('Browser.getBrowserCommandLine')).arguments;
  const profileIndex=argv.indexOf('--user-data-dir');
  const parsedProfile=parsed.includes(`--user-data-dir=${profile}`);
  // This Chromium exposes only its executable in /proc/cmdline after startup.
  // Verify its actual parsed flags through the private browser CDP endpoint,
  // require the exact launched PID, then let Rust check exe/starttime/ancestry.
  const exactProfile=parsedProfile&&pid===browser.pid;
  report.profile_identity={matches:exactProfile,pid_matches_launcher:pid===browser.pid,
    equals_flag:argv.some(arg=>arg.startsWith('--user-data-dir=')),split_flag:profileIndex>=0,
    proc_argument_count:argv.length,parsed_argument_count:parsed.length,parsed_profile_matches:parsedProfile};
  if(!exactProfile)throw Error('Browser profile identity mismatch');
  report.browser_product=(await browserCdp.call('Browser.getVersion')).product;
  const ticks=fs.readFileSync(`/proc/${pid}/stat`,'utf8').split(') ').at(-1).split(' ')[19];
  const exe=fs.readlinkSync(`/proc/${pid}/exe`);
  const targets=await (await fetch(`http://127.0.0.1:${port}/json/list`,{signal:AbortSignal.timeout(3000)})).json();
  const target=targets.find(item=>item.type==='page'&&item.url===`file://${html}`);
  if(!target)throw Error('Owned fixture target missing');
  pageCdp=await connect(target.webSocketDebuggerUrl);
  await pageCdp.call('Runtime.evaluate',{expression:'document.getElementById("probe").focus()'});
  let windows=[];
  while(Date.now()<deadline){windows=hypr('clients').filter(item=>item.pid===pid);if(windows.length===1)break;await delay(50);}
  if(windows.length!==1||!/^[0-9a-fx]+$/.test(windows[0].address))throw Error('Owned native window not unique');
  if(Boolean(windows[0].xwayland)!==(backend==='x11'))throw Error('Native backend mismatch');
  command('hyprctl',['dispatch',`hl.dsp.focus({ window = "address:${windows[0].address}" })`]);
  const focused=hypr('activewindow');
  if(focused.pid!==pid||focused.address!==windows[0].address)throw Error('Owned browser not focused');
  report.samples=[];
  for(let index=0;index<iterations;index++){
  await pageCdp.call('Runtime.evaluate',{expression:'document.getElementById("probe").value="";window.probe={input:false,paste:false};document.getElementById("probe").focus()'});
  driver=spawn(driverBinary,[String(pid),ticks,exe],{stdio:['pipe','pipe','ignore']});
  const lines=readline.createInterface({input:driver.stdout});
  const [line]=await Promise.race([once(lines,'line'),delay(4000).then(()=>{throw Error('Paste driver timeout');})]);
  report.driver=JSON.parse(line);
  const readbackDeadline=Date.now()+2000;
  do {
    const readback=await pageCdp.call('Runtime.evaluate',{expression:`({nativeInput:window.probe.input,nativePaste:window.probe.paste,matches:document.getElementById('probe').value===${JSON.stringify(expected)}})`,returnByValue:true});
    report.readback=readback.result.value;
    if(Object.values(report.readback).every(value=>value===true))break;
    await delay(30);
  }while(Date.now()<readbackDeadline);
  report.passed=report.driver.outcome==='sent_unconfirmed'&&Object.values(report.readback).every(value=>value===true);
  driver.stdin.end('finish\n');
  await once(driver,'exit');
  report.samples.push({index,driver:report.driver,readback:report.readback,passed:report.passed});
  if(!report.passed)throw Error('Native paste sample failed; no retry');
  }
  const durations=report.samples.map(sample=>sample.driver.delivery_call_ms).sort((a,b)=>a-b);
  report.delivery_call_p95_ms=durations[Math.ceil(durations.length*.95)-1];
  report.timing_scope='Actual production adapter copy+guards+native protocol acknowledgement, keymap already prepared; excludes controller persistence/UI/recording/ASR/LLM; not V11 end-to-end';
  report.driver_sha256=crypto.createHash('sha256').update(fs.readFileSync(driverBinary)).digest('hex');
} catch(error) {report.failure=error.message;}
finally {
  if(driver?.exitCode===null)driver.kill();
  try {await browserCdp?.call('Browser.close');}catch{}
  pageCdp?.socket.close();browserCdp?.socket.close();
  if(browser?.exitCode===null){await Promise.race([once(browser,'exit'),delay(3000)]);if(browser.exitCode===null)browser.kill();}
  try {
    if(types.length){
      const restore=spawn('python3',[path.join(root,'scripts/m0_desktop_probe.py'),'--restore'],{stdio:['pipe','pipe','ignore'],detached:true});
      const lines=readline.createInterface({input:restore.stdout});
      restore.stdin.end(JSON.stringify(Object.fromEntries(Object.entries(saved).map(([key,value])=>[key,value.toString('base64')])))+'\n');
      const [line]=await Promise.race([once(lines,'line'),delay(5000).then(()=>{throw Error('Clipboard restore timeout');})]);
      if(JSON.parse(line).event!=='restored')throw Error('Clipboard owner not restored');
      lines.close();restore.stdout.destroy();restore.unref();
      report.clipboard_restored=types.every(mime=>command('wl-paste',['--no-newline','--type',mime]).equals(saved[mime]));
    }else {command('wl-copy',['--clear']);report.clipboard_restored=true;}
  }catch{report.clipboard_restored=false;}
  report.passed&&=report.clipboard_restored;
  fs.rmSync(temporary,{recursive:true,force:true});
  fs.writeFileSync(output,JSON.stringify(report,null,2)+'\n',{flag:'wx'});
}
console.log(JSON.stringify({passed:report.passed,backend,iterations,samples_recorded:report.samples?.length||0,delivery_call_p95_ms:report.delivery_call_p95_ms,clipboard_restored:report.clipboard_restored,failure:report.failure}));
process.exitCode=report.passed?0:1;
