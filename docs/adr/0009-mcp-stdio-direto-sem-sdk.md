# ADR 0009 — Servidor MCP: stdio JSON-RPC implementado direto, sem SDK

**Status:** Aceito (jul/2026). Resolve a questão aberta do DESIGN §12 ("SDK MCP
`rmcp` vs. protocolo direto") no default previsto.

## Contexto

O item 1.4 do M1 expõe a engine como servidor MCP (`remember`/`recall`/`forget`).
O transporte decidido é stdio JSON-RPC (DESIGN §8). Duas rotas de implementação:

1. **SDK oficial Rust (`rmcp`)** — cobre o protocolo inteiro (resources, prompts,
   sampling, notificações de progresso…), mas é assíncrono por construção: puxa
   `tokio` completo + pilha de traits `Service`/`tower`-like, e está em evolução
   rápida (breaking changes entre releases). A engine é deliberadamente síncrona
   e sem tokio (DESIGN §10).
2. **Implementação direta** — o subconjunto do MCP que um servidor de tools
   precisa é minúsculo: `initialize`, `notifications/initialized`, `ping`,
   `tools/list`, `tools/call`, sobre JSON-RPC 2.0 com mensagens delimitadas por
   newline no stdio. Um loop de leitura síncrono resolve; stdio é
   inerentemente um cliente por processo, não há concorrência a gerenciar.

## Decisão

**Implementar o protocolo direto no `embedmind-mcp`, síncrono, com `serde_json`
como única dependência nova.**

- Loop: lê uma linha do stdin → parseia JSON-RPC → despacha → escreve uma linha
  no stdout. Logs vão para stderr (stdout é canal exclusivo do protocolo).
- Superfície: `initialize`, `notifications/initialized` (ignorada), `ping`,
  `tools/list`, `tools/call`. Método desconhecido → erro JSON-RPC `-32601`;
  JSON malformado → `-32700`; argumentos inválidos de tool → `-32602`.
- Falhas da engine em `tools/call` viram resultado com `isError: true` (conforme
  a spec MCP), nunca crash do servidor.
- O `clientInfo.name` do `initialize` é registrado como `agent` na proveniência
  básica das memórias gravadas via MCP (CLAUDE.md decisão 3).
- O núcleo do servidor é genérico sobre `BufRead`/`Write`, testável sem processo
  nem pipes reais.

Regra arquitetural inalterada (CLAUDE.md decisão 2): a casca contém zero lógica
de domínio — parse → chamada da API `embedmind-core` → serialize.

## Consequências

- Sem tokio/async em todo o workspace; binário menor; uma dependência nova
  (`serde_json`) em vez de uma árvore.
- Superfícies do MCP que não usamos (resources, prompts, sampling, cancelamento)
  ficam de fora até haver demanda — adicioná-las é incremental no mesmo loop.
- Mantemos nós a paridade com revisões futuras do protocolo (custo aceito: a
  superfície usada é pequena e estável; o handshake ecoa a `protocolVersion` do
  cliente quando suportada).
- Se um dia precisarmos do protocolo completo, migrar para SDK é reescrever a
  casca (~300 linhas), como previsto na decisão 2 do CLAUDE.md.

## Alternativas rejeitadas

- **`rmcp` (SDK oficial):** traz tokio + stack async para um servidor que
  atende um cliente por processo via stdio; API ainda instável entre releases;
  violaria o orçamento curto de dependências (DESIGN §10) para ganhar features
  que não usamos.
- **Outro SDK da comunidade:** menos maduro que o oficial, mesmo custo de
  dependências.
