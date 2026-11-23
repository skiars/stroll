// This program is embedded in both Stroll binaries and launched on the project host.
const { spawn } = require('node:child_process');
const { homedir } = require('node:os');
const { join } = require('node:path');
const { createRequire } = require('node:module');

const tools = join(process.env.STROLL_HOME || join(homedir(), '.stroll'), 'vue-tools');
const requireTool = createRequire(join(tools, 'package.json'));
let rpc, vueScript, tsScript, sdk;
try {
    rpc = requireTool('vscode-jsonrpc/node');
    vueScript = requireTool.resolve('@vue/language-server/bin/vue-language-server.js');
    tsScript = requireTool.resolve('typescript-language-server/lib/cli.mjs');
    sdk = requireTool.resolve('typescript/lib/tsserver.js');
    requireTool.resolve('@vue/typescript-plugin');
    if (!requireTool('typescript').server?.protocol) {
        throw new Error('Vue requires a JavaScript TypeScript SDK');
    }
} catch (error) {
    console.error(error.message);
    console.error('Install Vue language tools on this host:');
    console.error('npm install --prefix "' + tools + '" @vue/language-server@3.3.14 @vue/typescript-plugin@3.3.14 typescript-language-server@5.3.0 typescript@6.0.3 vscode-jsonrpc@8.2.1');
    process.exit(1);
}

const connection = (input, output) => rpc.createMessageConnection(
    new rpc.StreamMessageReader(input), new rpc.StreamMessageWriter(output),
);
const client = connection(process.stdin, process.stdout);
const processes = [];
let stopping = false;

function terminate() {
    stopping = true;
    for (const child of processes) child.kill();
}
process.on('exit', terminate);
process.on('SIGTERM', () => process.exit(0));
process.on('SIGINT', () => process.exit(0));
client.onClose(() => process.exit(0));

function server(script) {
    const child = spawn(process.execPath, [script, '--stdio'], {
        stdio: ['pipe', 'pipe', 'inherit'],
        windowsHide: true,
    });
    processes.push(child);
    child.on('error', error => {
        console.error(error.message);
        process.exit(1);
    });
    child.on('exit', (code, signal) => {
        if (!stopping) {
            console.error('Vue language tool exited: ' + script + ' (' + (signal ?? code) + ')');
            process.exit(1);
        }
    });
    child.stdin.on('error', error => {
        if (!stopping) console.error(error.message);
    });
    const peer = connection(child.stdout, child.stdin);
    peer.listen();
    return peer;
}

const vue = server(vueScript);
const ts = server(tsScript);
const peers = { vue, ts };
const typescriptDiagnostics = new Map();
const versions = new Map();
let vueCapabilities;
let tsCapabilities;
let tokenLegend;

function log(error) {
    if (!stopping) console.error(String(error));
}

async function forwardTsserver(params) {
    const [id, command, payload] = params[0];
    let body = null;
    try {
        const cancellation = new rpc.CancellationTokenSource();
        const timer = setTimeout(() => cancellation.cancel(), 8000);
        try {
            const response = await ts.sendRequest('workspace/executeCommand', {
                command: 'typescript.tsserverRequest',
                arguments: [command, payload],
            }, cancellation.token);
            body = response?.body ?? null;
        } finally {
            clearTimeout(timer);
            cancellation.dispose();
        }
    } catch (error) {
        log(error);
    }
    await vue.sendNotification('tsserver/response', [[id, body]]);
}

const isVue = uri => typeof uri === 'string' && /\.vue$/i.test(uri);
const configuredSettings = new Set(['vue', 'html', 'css', 'scss', 'less', 'typescript', 'javascript']);
for (const [name, peer] of Object.entries(peers)) {
    peer.onRequest(async (method, params) => {
        if (method === 'workspace/configuration') {
            const result = await client.sendRequest(method, params);
            return params.items.map((item, i) => result?.[i] ??
                (configuredSettings.has(item.section) ? {} : null));
        }
        return client.sendRequest(method, params);
    });
    peer.onNotification((method, params) => {
        if (method === 'tsserver/request' && name === 'vue') {
            void forwardTsserver(params).catch(log);
        } else if (method === 'textDocument/publishDiagnostics') {
            if (name === 'ts' && versions.has(params.uri)
                && (params.version == null || params.version === versions.get(params.uri))) {
                typescriptDiagnostics.set(params.uri, params.diagnostics);
                void client.sendRequest('workspace/diagnostic/refresh').catch(log);
            }
        } else if (method !== '$/typescriptVersion') {
            void client.sendNotification(method, params).catch(log);
        }
    });
}

client.onRequest('initialize', async params => {
    const [vueInit, tsInit] = await Promise.all([
        vue.sendRequest('initialize', { ...params, initializationOptions: {} }),
        ts.sendRequest('initialize', {
            ...params,
            initializationOptions: {
                plugins: [{ name: '@vue/typescript-plugin', location: tools, languages: ['vue'] }],
                // Quick info from the partial syntax server reports Vue template bindings as any.
                tsserver: { path: sdk, useSyntaxServer: 'never' },
                disableAutomaticTypingAcquisition: true,
            },
        }),
    ]);
    vueCapabilities = vueInit.capabilities;
    tsCapabilities = tsInit.capabilities;
    const legends = [vueCapabilities, tsCapabilities]
        .map(capabilities => capabilities.semanticTokensProvider?.legend).filter(Boolean);
    tokenLegend = legends.length ? {
        tokenTypes: [...new Set(legends.flatMap(legend => legend.tokenTypes))],
        tokenModifiers: [...new Set(legends.flatMap(legend => legend.tokenModifiers))],
    } : undefined;
    return {
        serverInfo: { name: 'Stroll Vue' },
        capabilities: {
            ...tsCapabilities,
            ...vueCapabilities,
            textDocumentSync: { openClose: true, change: 2, save: { includeText: true } },
            completionProvider: {
                resolveProvider: true,
                triggerCharacters: [...new Set([
                    ...(vueCapabilities.completionProvider?.triggerCharacters ?? []),
                    ...(tsCapabilities.completionProvider?.triggerCharacters ?? []),
                ])],
            },
            diagnosticProvider: { interFileDependencies: true, workspaceDiagnostics: false },
            semanticTokensProvider: tokenLegend && { legend: tokenLegend, full: true, range: true },
        },
    };
});
client.onRequest('shutdown', async () => {
    stopping = true;
    await Promise.all([ts.sendRequest('shutdown'), vue.sendRequest('shutdown')]);
    return null;
});

const documentNotifications = new Set([
    'textDocument/didOpen', 'textDocument/didChange', 'textDocument/didClose', 'textDocument/didSave',
]);
client.onNotification((method, params) => {
    if (method === 'exit') {
        Promise.allSettled(Object.values(peers).map(peer => peer.sendNotification(method)))
            .finally(() => process.exit(0));
        return;
    }
    const uri = params?.textDocument?.uri;
    if (method === 'textDocument/didOpen' || method === 'textDocument/didChange') {
        versions.set(uri, params.textDocument.version);
        typescriptDiagnostics.delete(uri);
    } else if (method === 'textDocument/didClose') {
        versions.delete(uri);
        typescriptDiagnostics.delete(uri);
    }
    void ts.sendNotification(method, params).catch(log);
    if (!documentNotifications.has(method) || isVue(uri)) {
        void vue.sendNotification(method, params).catch(log);
    }
});

function tagged(item, peer) {
    return { ...item, data: { strollVuePeer: peer, original: item.data } };
}

function unique(items, key) {
    const seen = new Set();
    return items.filter(item => {
        const id = key(item);
        if (seen.has(id)) return false;
        seen.add(id);
        return true;
    });
}

const vueOnly = new Set([
    'textDocument/formatting', 'textDocument/rangeFormatting', 'textDocument/onTypeFormatting',
    'textDocument/documentSymbol', 'textDocument/foldingRange', 'textDocument/documentLink',
    'textDocument/documentColor', 'textDocument/colorPresentation',
]);
const fallback = new Set([
    'textDocument/hover', 'textDocument/definition', 'textDocument/typeDefinition',
    'textDocument/implementation', 'textDocument/references', 'textDocument/rename',
    'textDocument/prepareRename', 'textDocument/signatureHelp', 'textDocument/documentHighlight',
    'textDocument/inlayHint',
]);
const completionItems = result => Array.isArray(result) ? result : result?.items ?? [];

client.onRequest(async (method, params, token) => {
    const request = (peer, args = params) => peers[peer].sendRequest(method, args, token);
    const maybe = async peer => {
        try {
            return await request(peer);
        } catch (error) {
            if (error.code !== -32601) throw error;
            return null;
        }
    };
    const uri = params?.textDocument?.uri;
    if (method === 'completionItem/resolve' || method === 'codeAction/resolve') {
        const source = params.data?.strollVuePeer ?? 'ts';
        const capability = method === 'completionItem/resolve' ? 'completionProvider' : 'codeActionProvider';
        const capabilities = source === 'vue' ? vueCapabilities : tsCapabilities;
        const item = params.data?.strollVuePeer ? { ...params, data: params.data.original } : params;
        return capabilities[capability]?.resolveProvider ? request(source, item) : item;
    }
    if (method === 'workspace/executeCommand') {
        return request(vueCapabilities.executeCommandProvider?.commands.includes(params.command) ? 'vue' : 'ts');
    }
    if (method === 'textDocument/diagnostic' && !isVue(uri)) {
        return { kind: 'full', items: typescriptDiagnostics.get(uri) ?? [] };
    }
    if (method === 'textDocument/semanticTokens/full' || method === 'textDocument/semanticTokens/range') {
        const peer = isVue(uri) ? 'vue' : 'ts';
        const legend = (peer === 'vue' ? vueCapabilities : tsCapabilities).semanticTokensProvider?.legend;
        if (!legend) return null;
        const result = await maybe(peer);
        if (result?.data) {
            const data = result.data.slice();
            for (let i = 0; i < data.length; i += 5) {
                data[i + 3] = tokenLegend.tokenTypes.indexOf(legend.tokenTypes[data[i + 3]]);
                const modifiers = data[i + 4];
                data[i + 4] = legend.tokenModifiers.reduce((bits, modifier, index) =>
                    modifiers & (1 << index)
                        ? bits | (1 << tokenLegend.tokenModifiers.indexOf(modifier))
                        : bits, 0) >>> 0;
            }
            return { data };
        }
        return null;
    }
    if (!isVue(uri)) return request('ts');
    if (method === 'textDocument/diagnostic') {
        const result = vueCapabilities.diagnosticProvider ? await maybe('vue') : null;
        return { kind: 'full', items: unique([
            ...(result?.items ?? []), ...(typescriptDiagnostics.get(uri) ?? []),
        ], item => JSON.stringify([item.range, item.code, item.message])) };
    }
    if (vueOnly.has(method)) return maybe('vue');
    if (method === 'textDocument/completion') {
        const [v, t] = await Promise.all([maybe('vue'), maybe('ts')]);
        return {
            isIncomplete: Boolean(v?.isIncomplete || t?.isIncomplete),
            items: unique([
                ...completionItems(v).map(item => tagged(item, 'vue')),
                ...completionItems(t).map(item => tagged(item, 'ts')),
            ], item => JSON.stringify([item.label, item.textEdit ?? item.insertText])),
        };
    }
    if (method === 'textDocument/codeAction') {
        const [v, t] = await Promise.all([maybe('vue'), maybe('ts')]);
        return [
            ...(v ?? []).map(item => tagged(item, 'vue')),
            ...(t ?? []).map(item => tagged(item, 'ts')),
        ];
    }
    if (fallback.has(method)) {
        const result = await maybe('ts');
        if (result != null && (!Array.isArray(result) || result.length)) return result;
    }
    return maybe('vue');
});

client.listen();
