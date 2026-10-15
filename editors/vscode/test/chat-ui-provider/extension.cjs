const vscode=require('vscode');
const fs=require('node:fs');
exports.activate=async context=>{
 const event=(value)=>fs.appendFileSync(process.env.MINI_AGENT_CHAT_FIXTURE_EVENTS,JSON.stringify(value)+'\n');
 context.subscriptions.push(vscode.lm.registerLanguageModelChatProvider('mini-agent-offline-fixture',{
  provideLanguageModelChatInformation(){event({stage:'model-information'});return [{id:'offline-ide',name:'Owned Offline Fixture',family:'offline-fixture',version:'1',maxInputTokens:8192,maxOutputTokens:256,capabilities:{toolCalling:true}}];},
  async provideLanguageModelChatResponse(_model,_messages,_options,progress){event({stage:'fixture-model-response'});progress.report(new vscode.LanguageModelTextPart('LOCAL_FIXTURE_MODEL_ONLY'));},
  async provideTokenCount(_model,text){return Math.ceil((typeof text==='string'?text:JSON.stringify(text)).length/4);}
 }));
 event({stage:'provider-registered'});
 const models=await vscode.lm.selectChatModels({vendor:'mini-agent-offline-fixture'});event({stage:'models-selected',models:models.map(m=>({id:m.id,vendor:m.vendor}))});
};
