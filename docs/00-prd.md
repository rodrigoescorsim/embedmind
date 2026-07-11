# PRD — EmbedMind: memória persistente para agentes de IA

> Documento canônico do pacote SDD (00 de 04). Destilado manualmente em 08/jul/2026 a
> partir do material real do projeto (README, CLAUDE.md, ROADMAP, plano open-core
> jul/2026) para integrar o EmbedMind à esteira agêntica do Painel. Em conflito com
> este documento, vale o estado mais recente de [ROADMAP.md](../ROADMAP.md).

## 1. Visão e pitch

**"Your agent forgets everything between sessions. This fixes that."**

EmbedMind é **memória persistente para agentes de IA**: uma engine de armazenamento
embarcada (vetor + full-text + grafo) empacotada como **servidor MCP de memória + CLI**.
O *SQLite da memória de agente*: um único arquivo local crash-safe, in-process, sem
servidor, sem nuvem, sem Python — Rust puro, um binário.

A porta de entrada é `remember` / `recall` / `forget` plug-and-play em qualquer host MCP
(Claude Code, Cursor, agentes custom). A engine é o ativo; o MCP é casca descartável.

## 2. Problema e público-alvo

**Problema:** a dor nº 1 de agentes de código é **amnésia entre sessões**, hoje
re-resolvida com arquivos markdown frágeis. As alternativas de armazenamento são bancos
vetoriais server-based (pesados, mais um processo para babá) ou stores embarcados
vector-only. Não existe o equivalente do SQLite para memória de agente: embarcável,
arquivo único, vetor + texto + grafo juntos, criptografável, do desktop ao mobile.

**Público, em ondas:**

1. **Dev individual usando coding agents** (Claude Code, Cursor) — adoção self-service
   via GitHub. É quem dá estrela, abre issue e valida.
2. **Times de agentes custom** (LangChain e afins, destravados pelos bindings Python
   no M2) — multiplicador de audiência.
3. **Ambientes regulados** (banco/fintech): descobrem via GitHub; exigem criptografia,
   RBAC, auditoria e air-gap — direção futura (fora do escopo v0.1, criptografia
   reservada no formato desde o dia 1).

## 3. Proposta de valor e diferencial

| Promessa | Sustentação técnica |
|---|---|
| Um arquivo, portátil | formato `.mind` paginado, WAL, spec pública byte a byte |
| Nunca corrompe sua memória | crash-harness determinístico + fuzzing no CI desde o dia 1 — **confiabilidade É o moat** |
| In-process, zero operação | sem servidor, sem Docker, sem portas |
| Local por default | zero chamada de rede no núcleo — auditável no código; air-gap ready |
| Semântica sem API key | embeddings ONNX embarcados (MiniLM int8, CPU-only) |
| Híbrido de verdade | vetor (HNSW paginado) + full-text BM25 com fusão RRF + filtros de metadados + grafo de entidades/relações — trio COMPLETO E ENTREGUE (jul/2026, antes do cronograma); nenhum embarcado tem os três juntos |
| Conhecimento versionado (fase FR, pré-launch) | `supersedes` de primeira classe + recência na fusão + curadoria na escrita (stories S19–S21) — corrigir um fato sem perder o histórico; nenhum concorrente embarcado tem semântica de versão de conhecimento |

**Por que este produto ganha:** a barreira técnica (storage engine crash-safe + HNSW
paginado próprio em Rust) é exatamente o perfil do founder (dev sênior C++/Rust, solo) e
não é replicável por wrapper de fim de semana. O posicionamento "memória para agentes"
(não "database para RAG") foge da vala comum onde sqlite-vec/LanceDB/zvec competem.
Honestidade técnica como marca: benchmarks publicados **incluindo onde perdemos**.

## 4. Métricas de sucesso (mensuráveis, com prazo)

Linha do tempo concreta (M1 iniciado em 07/jul/2026): **launch público dia 35 ≈
11/ago/2026 (hard stop)** · alarme "repo ainda privado" dia 45 ≈ 21/ago · **go/no-go dia
90 ≈ 05/out/2026**.

Métricas do go/no-go (~7 semanas pós-launch):

| Métrica | 🔴 Fraco | 🟡 Bom | 🟢 Forte | O que mede |
|---|---|---|---|---|
| Estrelas | < 300 | 300–1.500 | > 1.500 | ressonância da mensagem |
| Issues/discussões de terceiros | < 10 | 10–40 | > 40 | **uso real** |
| PRs externos aceitos | 0 | 1–5 | > 5 | comunidade nascendo |
| Downloads recorrentes/semana | < 100 | 100–1.000 | > 1.000 | retenção |

**Regras de decisão (compromisso prévio):** 2+ colunas 🟢 (sendo uma *issues*) → GO
para M4–M6. Maioria 🟡 → mais 90 dias no núcleo OSS com um
reposicionamento. Maioria 🔴 com launch bem executado → reempacotar a mesma engine com
outra porta de entrada; só após 2 empacotamentos fracos a tese se considera refutada.

## 5. Escopo do MVP (v0.1 / M1) vs. fora de escopo

**Dentro da v0.1:** arquivo único + WAL crash-safe · KV store + API Rust ·
busca vetorial HNSW paginada + embeddings ONNX embarcados (com chunking de memórias
longas) · MCP `remember`/`recall`/`forget` · memória automática de contexto de projeto ·
CLI (`serve`/`remember`/`recall`/`forget`/`stats`) · instalação em 1 comando ·
testes de crash-recovery + fuzzing no CI.

**Fora da v0.1 — cada exclusão com justificativa:**

| Excluído | Por quê |
|---|---|
| Full-text + filtros de metadados | M2 — vetor sozinho já entrega o "aha"; full-text sem usuários é polimento prematuro |
| Camada de grafo | M3 — é o diferencial de profundidade, não de entrada |
| Criptografia/RBAC/auditoria | pós-90 dias — mas **reservados no formato** desde o dia 1 (não quebrar formato depois) |
| Sync/equipe | pós-90 dias — sem demanda comprovada ainda |
| Vacuum online / compactação | `forget` é soft-delete; vacuum offline chega na v0.2 |
| Multi-processo escritor / MVCC | single-writer cobre o caso real (um agente por arquivo) |
| Bindings Python/TS | M2/M4 — exigem API estável primeiro |

## 6. Riscos de produto e mitigação

| Risco | Mitigação |
|---|---|
| Commoditização (sqlite-vec/LanceDB/zvec com times pagos) | 4 semanas até usável; posicionamento "memória para agentes", não "database" |
| MCP perder relevância | engine em camadas; casca MCP substituível (~300 linhas) |
| Bug de corrupção de dados (mata o moat) | WAL antes de features; fuzzing + crash tests no CI; postmortem público em caso de incidente |
| Burnout OSS (founder solo) | SLA "best effort" público; releases em ritmo fixo quinzenal; feature grande só com 2+ pedidos |
| Launch de primeira vez (sem satélite de calibração) | material de launch (post, GIF, FAQ) preparado dentro do M1, não na véspera do dia 35 |

## Adendo — 2026-07-10 (estado real + fase FR)

- **M2 e M3 foram entregues antes do cronograma** (full-text S9, filtros S10, vacuum
  S11, bindings Python S12/B5, grafo S13, proveniência S14). As tabelas dos §5–§6
  acima refletem o plano original de 08/jul — o estado vivo por story está em
  [01-spec.md](01-spec.md); em conflito, vale a spec.
- **Nova fase pré-launch FR (frescor do conhecimento + observabilidade)**, decidida
  pelo founder em 10/jul a partir do dogfooding via Painel Agêntico (o EmbedMind é a
  memória do agente que desenvolve o próprio EmbedMind): `supersedes` de primeira
  classe (S19), recência na fusão do recall (S20), curadoria na escrita (S21) e
  op-log estruturado (S22). **"Conhecimento versionado" entra como diferencial do
  anúncio de launch** — nenhum store embarcado concorrente tem semântica de versão
  de conhecimento.
- Motivação técnica dos achados: o ranking (RRF sobre vetor+texto) não tem componente
  temporal — memória defasada semanticamente próxima vence a correção mais nova; as
  relações `contradicts`/`refines` são navegacionais e o recall não age sobre elas;
  não há log estruturado de operações e o lock exclusivo impede inspeção concorrente.
