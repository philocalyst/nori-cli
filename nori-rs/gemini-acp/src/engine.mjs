// Gemini Core is the execution engine. ACP and process ownership live in Rust.
import { createInterface } from 'node:readline';
import { randomUUID } from 'node:crypto';
import { pathToFileURL } from 'node:url';
const write = (message) => process.stdout.write(`${JSON.stringify(message)}\n`);
// Core uses console output for diagnostics; reserve stdout for the protocol.
for (const level of ['log', 'info', 'warn', 'debug']) console[level] = (...args) => console.error(...args);
const core = await import(pathToFileURL(process.env.NORI_GEMINI_CORE_MODULE).href);
let config, client, controller, active = false;
// Core reports token counts on each Finished event; sum them over the prompt's model calls.
let usage;
const addUsage = (metadata) => {
  const sum = usage ?? (usage = {prompt: 0, candidates: 0, cached: 0, thoughts: 0});
  sum.prompt += metadata.promptTokenCount ?? 0; sum.candidates += metadata.candidatesTokenCount ?? 0;
  sum.cached += metadata.cachedContentTokenCount ?? 0; sum.thoughts += metadata.thoughtsTokenCount ?? 0;
  sum.lastPrompt = metadata.promptTokenCount ?? 0;
};
const done = (reason) => {
  const report = usage && {...usage, model: config.getModel(), contextSize: core.tokenLimit?.(config.getModel())};
  write(report ? {type:'done', reason, usage:report} : {type:'done', reason});
};
const pending = new Set();
const seenPermissions = new Set();
const toolStatuses = new Map();
async function init(message) {
  config = new core.Config({
    sessionId: message.sessionId, clientName: 'nori', clientVersion: '1',
    cwd: message.cwd, targetDir: message.cwd, model: message.model ?? process.env.GEMINI_MODEL ?? core.DEFAULT_GEMINI_MODEL, includeDirectories: message.additionalDirectories ?? [],
    debugMode: false, interactive: true, folderTrust: true,
    usageStatisticsEnabled: false, telemetry: {enabled: false},
    fakeResponses: message.fakeResponses, skipNextSpeakerCheck: true,
    enableAgents: false, skillsSupport: false, mcpEnabled: true, mcpServers: message.mcpServers ?? {},
    extensionsEnabled: false, enableHooks: false, plan: false, tracker: false,
    includeDirectoryTree: false, checkpointing: false,
  });
  await config.initialize();
  const authTypes = {gemini: core.AuthType.USE_GEMINI, vertex: core.AuthType.USE_VERTEX_AI};
  if (!Object.hasOwn(authTypes, message.auth)) throw new Error('Unsupported Gemini authentication mode');
  await config.refreshAuth(authTypes[message.auth]);
  client = config.getGeminiClient();
  await client.initialize();
  config.getMessageBus().subscribe(core.MessageBusType.TOOL_CALLS_UPDATE, ({toolCalls}) => {
    for (const call of toolCalls) {
      const {callId, name, args, display} = call.request;
      if (call.status === 'awaiting_approval' && !seenPermissions.has(call.correlationId)) {
        pending.add(call.correlationId);
        seenPermissions.add(call.correlationId);
        write({type:'permission', id:call.correlationId, callId, name, args, display, details:call.confirmationDetails});
      }
      if (['executing', 'success', 'error', 'cancelled'].includes(call.status) && toolStatuses.get(callId) !== call.status) {
        toolStatuses.set(callId, call.status);
        // Core exposes model response parts, tool data, and display separately.
        // Preserve the complete response rather than reducing it to display text.
        write({type:'tool', callId, name, args, display, status:call.status, result:call.response});
      }
    }
  });
  write({type:'ready', model:config.getModel(), models:[...core.VALID_GEMINI_MODELS]});
}
async function prompt(message) {
  if (active) throw new Error('Prompt already active');
  active = true; controller = new AbortController(); usage = undefined;
  try {
    let parts = message.parts;
    while (true) {
      const calls = [];
      let stopReason = "end_turn";
      for await (const event of client.sendMessageStream(parts, controller.signal, randomUUID())) {
        if (event.type === core.GeminiEventType.Finished && event.value?.usageMetadata) addUsage(event.value.usageMetadata);
        if (event.type === core.GeminiEventType.ToolCallRequest) { calls.push(event.value); write({type:'tool', callId:event.value.callId, name:event.value.name, args:event.value.args, display:event.value.display, status:'pending'}); }
        else if (event.type === core.GeminiEventType.Content || event.type === core.GeminiEventType.Thought) write({type:event.type, value:event.value});
        else if (event.type === core.GeminiEventType.Error) throw new Error(JSON.stringify(event.value));
        else if (event.type === core.GeminiEventType.UserCancelled) controller.abort();
        else if (event.type === core.GeminiEventType.MaxSessionTurns) stopReason = 'max_turn_requests';
        else if (event.type === core.GeminiEventType.Finished && event.value?.reason === 'MAX_TOKENS') stopReason = 'max_tokens';
        else if (event.type === core.GeminiEventType.Finished && ['SAFETY', 'RECITATION', 'BLOCKLIST', 'PROHIBITED_CONTENT'].includes(event.value?.reason)) stopReason = 'refusal';
        else if ([core.GeminiEventType.LoopDetected, core.GeminiEventType.InvalidStream, core.GeminiEventType.AgentExecutionBlocked, core.GeminiEventType.AgentExecutionStopped].includes(event.type)) throw new Error(`Gemini stopped: ${event.type}`);
      }
      if (controller.signal.aborted) { done('cancelled'); return; }
      if (!calls.length) { done(stopReason); return; }
      const completed = await core.scheduleAgentTools(config, calls, {schedulerId:'root', toolRegistry:config.getToolRegistry(), signal:controller.signal});
      parts = completed.flatMap((call) => call.response.responseParts);
    }
  } catch (error) {
    if (controller.signal.aborted) done('cancelled');
    else throw error;
  } finally { active = false; controller = undefined; pending.clear(); seenPermissions.clear(); toolStatuses.clear(); }
}
createInterface({input:process.stdin}).on('line', (line) => {
  let message;
  try { message = JSON.parse(line); } catch { write({type:'error', message:'Invalid engine request'}); return; }
  if (message.type === 'set_model') { if (active) write({type:'error', message:'Prompt already active'}); else { config.setModel(message.model); write({type:'configured', model:config.getModel()}); } return; }
  if (message.type === 'cancel') { controller?.abort(); return; }
  if (message.type === 'permission') {
    if (pending.delete(message.id)) setImmediate(() => config.getMessageBus().publish({type:core.MessageBusType.TOOL_CONFIRMATION_RESPONSE, correlationId:message.id, confirmed:message.allow, outcome:message.allow ? core.ToolConfirmationOutcome.ProceedOnce : core.ToolConfirmationOutcome.Cancel}));
    return;
  }
  const operation = message.type === 'init' ? init(message) : message.type === 'prompt' ? prompt(message) : Promise.reject(new Error('Unknown engine request'));
  operation.catch((error) => write({type:'error', message:String(error?.message ?? error)}));
});
