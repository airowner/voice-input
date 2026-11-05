'use strict';
const $ = id => document.getElementById(id);
let current = null;
let editing = false;
let refreshing = false;
let paintedResults = '';
const invoke = (name, args = {}) => {
  if (!window.__TAURI__) return Promise.reject(new Error('请在声笺桌面应用中打开此页面。'));
  return window.__TAURI__.core.invoke(name, args);
};
function showError(error) {
  $('error').textContent = error?.message || String(error);
  $('error').hidden = false;
}
async function execute(command) {
  try {
    $('error').hidden = true;
    const result = await invoke('execute', { command });
    await refresh();
    return result;
  } catch (error) { showError(error); throw error; }
}
function option(value, text) {
  const node = document.createElement('option'); node.value = value; node.textContent = text; return node;
}
function setOptions(id, entries, selected) {
  const select = $(id); select.replaceChildren(...entries.map(e => option(e.value, e.text))); select.value = selected || '';
}
function refreshModelChoices(data) {
  for(const [id,engine] of [['asr-model','whisper'],['llm-model','gguf']]){
    const select=$(id);const selected=select.value;
    const entries=[{value:'',text:'请选择已验证模型'},...(data.models || []).filter(m=>m.engine===engine).map(m=>({value:m.id,text:m.name||m.id}))];
    if(JSON.stringify([...select.options].map(o=>o.value))!==JSON.stringify(entries.map(e=>e.value)))setOptions(id,entries,selected);
  }
}
function switchPanel(id) {
  document.querySelectorAll('.panel').forEach(p => { p.hidden = p.id !== id; });
  document.querySelectorAll('.tab').forEach(p => p.classList.toggle('active', p.dataset.panel === id));
}
function modes() {
  const local=$('llm-mode').value==='local';$('local-settings').hidden=!local;$('remote-settings').hidden=local;
  document.querySelectorAll('#local-settings input,#local-settings select,#local-settings button').forEach(n=>{n.disabled=!local;});
  document.querySelectorAll('#remote-settings input,#remote-settings select,#remote-settings button').forEach(n=>{n.disabled=local;});
}
function confirmAction(title,text) {
  if($('confirmation').open)return Promise.resolve(false);
  return new Promise(resolve=>{
    const dialog=$('confirmation');$('confirmation-title').textContent=title;$('confirmation-text').textContent=text;
    const finish=value=>{dialog.close();$('confirm-yes').onclick=null;$('confirm-no').onclick=null;resolve(value);};
    $('confirm-yes').onclick=()=>finish(true);$('confirm-no').onclick=()=>finish(false);dialog.oncancel=event=>{event.preventDefault();finish(false);};dialog.showModal();
  });
}
function paintModelGuidance(data) {
  const facts=data.hardware;const hints=data.model_recommendations || [];const nodes=[];
  const gib=value=>Number.isFinite(value)?`${(value/1073741824).toFixed(1)} GiB`:'未知';
  if(facts){
    nodes.push(textNode('strong','启动时资源建议（不会自动修改设置）'));
    nodes.push(textNode('p',`${facts.architecture} · 可用逻辑 CPU：${facts.logical_cpus ?? '未知'} · 总内存：${gib(facts.total_memory_bytes)} · 启动时可用：${gib(facts.available_memory_bytes)} · 剩余磁盘：${gib(facts.available_disk_bytes)}`));
    nodes.push(textNode('p',`后端：${(facts.available_backends || []).join('、') || '未知'} · GPU：${facts.gpu_status || '未探测'} · 内存来源：${facts.memory_source || '未知'}`));
  }
  for(const hint of hints)nodes.push(textNode('p',`${hint.reason}${Number.isFinite(hint.estimated_memory_bytes)?` 粗略预算 ${gib(hint.estimated_memory_bytes)}。`:''}`));
  for(const source of new Set(hints.map(hint=>hint.estimate_source).filter(Boolean)))nodes.push(textNode('p',source));
  $('model-guidance').replaceChildren(...nodes);$('model-guidance').hidden=nodes.length===0;
}
function paintConfig(data) {
  const c = data.config;
  setOptions('device', [{value:'default',text:'系统默认'}, ...(data.devices || []).map(d => ({value:d.id,text:d.name}))], c.audio.device_id);
  const models = data.models || [];
  for (const [id, engine, selected] of [['asr-model','whisper',c.asr.model_id], ['llm-model','gguf',c.llm.local.model_id]]) {
    setOptions(id, [{value:'',text:'请选择已验证模型'}, ...models.filter(m => m.engine === engine).map(m => ({value:m.id,text:m.name || m.id}))], selected);
  }
  $('language').value=c.asr.language; $('llm-mode').value=c.llm.mode; $('profile').value=c.llm.profile;
  $('max-seconds').value=c.audio.max_seconds;$('asr-threads').value=c.asr.threads;
  setOptions('asr-backend',[{value:'auto',text:'自动（CPU）'},{value:'cpu',text:'CPU'},...(!['auto','cpu'].includes(c.asr.backend)?[{value:c.asr.backend,text:`当前构建未验证 ${c.asr.backend}，请选择 CPU`}]:[])],c.asr.backend);
  $('asr-timeout').value=c.asr.timeout_seconds;$('gpu-layers').value=c.llm.local.gpu_layers;
  $('context-tokens').value=c.llm.local.context_tokens;$('llm-threads').value=c.llm.local.threads;$('local-timeout').value=c.llm.local.timeout_seconds;
  $('base-url').value=c.llm.remote.base_url; $('remote-model').value=c.llm.remote.model;
  $('remote-timeout').value=c.llm.remote.timeout_seconds; $('delivery').value=c.delivery.mode;
  $('persist').checked=c.recovery.persist; $('glossary').value=(c.llm.glossary || []).join('\n');
  $('max-entries').value=c.recovery.max_entries;$('ttl-hours').value=c.recovery.ttl_hours;
  $('remote-consent').checked=Boolean(data.remote_confirmed);
  $('credential-status').textContent=data.credential_present?'此地址已有凭据，密钥不可回读。':'此地址尚未配置凭据。';
  modes();
  paintModelGuidance(data);
  $('model-list').replaceChildren(...models.map(model=>{
    const row=document.createElement('div');row.className='model-row';
    const description=document.createElement('div');description.append(textNode('strong',model.name),
      textNode('p',`${model.engine === 'whisper' ? 'Whisper' : 'GGUF'} · 版本：${model.version || '未记录'} · 语言：${model.language || '未声明'}`,'hint'),
      textNode('p',`${(model.bytes/1048576).toFixed(0)} MiB · ${model.license} · SHA-256 ${model.sha256}`,'hint'),
      textNode('p',`来源：${model.source || '未记录'}`,'hint'),
      textNode('p',`兼容范围：${model.compatibility || '未声明，请以实际加载验证为准'}`,'hint'));
    const remove=textNode('button','删除');remove.type='button';remove.className='quiet';remove.disabled=model.id===c.asr.model_id||model.id===c.llm.local.model_id||data.operation_busy;
    remove.onclick=()=>execute({type:'delete_model',model_id:model.id}).catch(()=>{});row.append(description,remove);return row;
  }));
}
function textNode(tag, value, className) { const n=document.createElement(tag); n.textContent=value; if(className)n.className=className; return n; }
function resultCard(record) {
  const card=document.createElement('article');card.className='result';
  const head=document.createElement('div');head.className='result-head';
  const deliveries={not_attempted:'尚未交付',preview:'预览',attempting:'正在尝试交付',sent_unconfirmed:'已发送粘贴（未确认）',manual:'请手动复制粘贴',unknown:'交付未知，请先检查目标'};
  head.append(textNode('span', new Date(record.created_at_ms).toLocaleString()),textNode('span',deliveries[record.delivery] || '预览','badge'));
  card.append(head);
  if(record.processed){card.append(textNode('h3','整理结果'),textNode('div',record.processed,'result-text'));}
  card.append(textNode('h3','原始转写'),textNode('div',record.raw,'result-text raw'));
  const actions=document.createElement('div');actions.className='actions';
  const add=(label,type,extra={})=>{const b=textNode('button',label);b.addEventListener('click',()=>execute({type,job_id:record.job_id,...extra}).catch(()=>{}));actions.append(b);};
  if(record.processed)add('复制整理结果','copy_result',{processed:true});
  add('复制原文','copy_result',{processed:false});add('重新整理（先预览）','retry');add('删除','delete_result');
  if(current&&JSON.stringify(record.config?.llm)!==JSON.stringify(current.config.llm)){
    const retry=textNode('button','用当前设置重新整理');retry.onclick=async()=>{
      const c=current.config;const target=c.llm.mode==='remote'?`${c.llm.remote.base_url}\n模型：${c.llm.remote.model}`:`本地模型：${c.llm.local.model_id}`;
      if(await confirmAction('使用当前处理档案？',`${target}\n档案：${c.llm.profile}\n结果会先进入预览。`))execute({type:'retry',job_id:record.job_id,use_current_config:true}).catch(()=>{});
    };actions.append(retry);
  }
  card.append(actions);return card;
}
function paint(data) {
  current=data;
  const phase=data.state?.phase || 'ready';
  const labels={ready:'准备就绪',recording:'正在录音',transcribing:'正在本地转写',rewriting:'正在整理文字',delivering:'正在交付',result:'结果已保留'};
  $('phase').textContent=data.operation_busy?'正在处理配置任务':phase==='ready'&&!data.ready?'需要完成配置':labels[phase] || phase;
  $('phase-detail').textContent=data.detail || '原文先保存，再整理；取消不会丢失已提交的原文。';
  $('status-dot').classList.toggle('recording',phase==='recording');
  $('start').disabled=!['ready','result'].includes(phase)||!data.ready;
  $('stop').disabled=phase!=='recording';$('cancel').disabled=!data.operation_busy&&!['recording','transcribing','rewriting','delivering'].includes(phase);
  $('preflight').replaceChildren(...[['麦克风',data.microphone_ready],['转写模型',data.asr_ready],['文字整理',data.llm_ready]].map(([name,ready])=>textNode('div',(ready?'✓ ':'○ ')+name,'check-item'+(ready?' ready':''))));
  if(!editing)paintConfig(data);else refreshModelChoices(data);
  $('test-remote').disabled=editing||data.operation_busy||$('llm-mode').value!=='remote';
  $('delete-key').disabled=editing||data.operation_busy||$('llm-mode').value!=='remote';
  $('delivery-capability').textContent=data.capabilities?.automatic_delivery_verified?'本次会话已通过专用框回读验证。':'尚未验证，当前保留预览或手动交付。';
  if(editing)$('credential-status').textContent='先保存设置，再测试已保存端点或删除其凭据。';
  const records=data.recent || [];
  const key=JSON.stringify([records,data.config.llm]);
  if(key!==paintedResults){paintedResults=key;if(records.length)$('results').replaceChildren(...records.map(resultCard));else $('results').replaceChildren(textNode('div','还没有结果。开始一次录音，让想法落在这里。','empty'));}
  if(data.state?.error)showError(data.state.error);
}
async function refresh() {
  if(refreshing)return;
  refreshing=true;
  try{paint(await invoke('snapshot'));}catch(error){if(error?.code!=='starting')showError(error);}finally{refreshing=false;}
}
document.querySelectorAll('.tab').forEach(b=>b.addEventListener('click',()=>switchPanel(b.dataset.panel)));
$('llm-mode').addEventListener('change',modes);
function markDirty(){editing=true;$('saved').textContent='有未保存的设置';$('test-remote').disabled=true;$('delete-key').disabled=true;}
$('config-form').addEventListener('input',markDirty);
$('config-form').addEventListener('change',markDirty);
$('base-url').addEventListener('input',()=>{$('remote-consent').checked=false;$('api-key').value='';$('credential-status').textContent='地址已改变，需要重新确认并配置凭据。';});
for(const type of ['start','stop','cancel'])$(type).addEventListener('click',()=>execute({type}).catch(()=>{}));
$('clear').addEventListener('click',()=>execute({type:'clear_history'}).catch(()=>{}));
document.querySelectorAll('[data-import]').forEach(b=>b.addEventListener('click',()=>execute({type:'import_model',engine:b.dataset.import}).catch(()=>{})));
document.querySelectorAll('[data-download]').forEach(b=>b.addEventListener('click',async()=>{
  try{const info=await invoke('download_info',{modelId:b.dataset.download});if(await confirmAction(`下载 ${info.name}`,`来源：${info.source}\n体积：${(info.bytes/1048576).toFixed(1)} MiB\n许可：${info.license}`))await execute({type:'download_model',model_id:b.dataset.download});}catch(e){showError(e);}
}));
$('config-form').addEventListener('submit',async event=>{
  event.preventDefault();if(!current)return;
  const config=structuredClone(current.config);
  config.audio.device_id=$('device').value;config.asr.model_id=$('asr-model').value;config.asr.language=$('language').value;
  config.audio.max_seconds=Number($('max-seconds').value);config.asr.threads=Number($('asr-threads').value);
  config.asr.backend=$('asr-backend').value;config.asr.timeout_seconds=Number($('asr-timeout').value);
  config.llm.mode=$('llm-mode').value;config.llm.profile=$('profile').value;
  if(config.llm.mode==='local'){
    config.llm.local.model_id=$('llm-model').value;config.llm.local.gpu_layers=Number($('gpu-layers').value);
    config.llm.local.context_tokens=Number($('context-tokens').value);config.llm.local.threads=Number($('llm-threads').value);config.llm.local.timeout_seconds=Number($('local-timeout').value);
  }else{
    config.llm.remote.base_url=$('base-url').value.trim();config.llm.remote.model=$('remote-model').value.trim();config.llm.remote.timeout_seconds=Number($('remote-timeout').value);
  }
  config.delivery.mode=$('delivery').value;config.recovery.persist=$('persist').checked;config.llm.glossary=$('glossary').value.split('\n').map(s=>s.trim()).filter(Boolean);
  config.recovery.max_entries=Number($('max-entries').value);config.recovery.ttl_hours=Number($('ttl-hours').value);
  const key=config.llm.mode==='remote'?$('api-key').value:'';$('api-key').value='';
  const confirmed=config.llm.mode==='remote'?$('remote-consent').checked:current.remote_confirmed;
  try{await execute({type:'save_config',config,remote_confirmed:confirmed,key:key||null,session_only:$('session-key').checked});editing=false;$('saved').textContent='设置已保存';await refresh();}catch{ /* shown by execute */ }
});
$('test-remote').addEventListener('click',()=>execute({type:'test_remote'}).then(()=>$('saved').textContent='正在测试，结果显示在录音页状态中').catch(()=>{}));
$('delete-key').addEventListener('click',()=>execute({type:'delete_credential'}).catch(()=>{}));
$('refresh-diagnostics').addEventListener('click',async()=>{try{$('diagnostic-data').textContent=JSON.stringify(await invoke('diagnostics'),null,2);}catch(e){showError(e);}});
$('open-delivery-probe').addEventListener('click',()=>{if(!$('delivery-probe').open){$('delivery-probe-status').textContent='';$('delivery-probe-input').value='';$('delivery-probe').showModal();}});
async function closeDeliveryProbe(){await execute({type:'cancel_delivery_test'}).catch(()=>{});$('delivery-probe').close();}
$('close-delivery-probe').addEventListener('click',closeDeliveryProbe);
$('delivery-probe').addEventListener('cancel',event=>{event.preventDefault();closeDeliveryProbe();});
$('begin-delivery-probe').addEventListener('click',async()=>{
  const button=$('begin-delivery-probe');button.disabled=true;
  const input=$('delivery-probe-input');input.value='';input.focus();$('delivery-probe-status').textContent='保持此窗口和验证框有焦点…';
  try{
    await new Promise(resolve=>setTimeout(resolve,50));
    if(!$('delivery-probe').open)return;
    const result=await execute({type:'test_delivery'});
    const deadline=Date.now()+2000;
    while(input.value!==result.expected&&Date.now()<deadline)await new Promise(resolve=>setTimeout(resolve,25));
    await execute({type:'complete_delivery_test',token:result.token,observed_text:input.value});
    $('delivery-probe-status').textContent='验证成功。实际任务仍会逐次检查焦点和目标。';
  }catch(error){$('delivery-probe-status').textContent=error?.message||String(error);await execute({type:'cancel_delivery_test'}).catch(()=>{});}
  finally{button.disabled=false;}
});
refresh();setInterval(refresh,1000);
