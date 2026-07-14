# ROADMAP — EmbedMind

Plano técnico derivado do plano de construção open-core de jul/2026 (`plano_construcao_open_core_2026_07.html`). Princípios: lançar antes de estar orgulhoso · release a cada 2–3 semanas · todo marco termina em algo público · um único projeto em janela de lançamento ativo por vez.

## Visão geral das fases

```
Semana 0        M1 (sem. 1–4)      M2 (sem. 5–8)       M3 (sem. 9–12)      Pós-90 dias
README battle → núcleo mínimo   → lançamento público → grafo + proveniência → M4–M6 (se GO)
(gate: decisão)  (repo privado)    (dia 35, hard stop)  (go/no-go dia 90)   extensões + vitrine
```

> **Nota (atualizada em 07/jul/2026):** o satélite de calibração **AgentLock foi removido do caminho** por decisão do founder — o M1 começa imediatamente após a semana 0, e o lançamento do EmbedMind é a estreia da máquina de lançamento. Consequência assumida: os erros de primeira vez (post, Show HN, cadência de resposta) serão queimados no próprio EmbedMind; mitigação: preparar o material de launch (post, GIF, FAQ) com antecedência dentro do M1, não na véspera do dia 35.
>
> **Linha do tempo concreta (M1 iniciando em 07/jul/2026):** dia 35 (launch público, hard stop) ≈ **11/ago** · alarme "repo ainda privado" no dia 45 ≈ **21/ago** · go/no-go do dia 90 ≈ **05/out**.

---

## Semana 0 — Gate de decisão (README-driven development)

- [x] Escrever o README impecável do EmbedMind (este repo) — pitch, GIF imaginado.
- [x] Compará-lo com o README do Enclave (desafiante, mesma barreira técnica de sistemas).
- [x] **Decisão da aposta principal ao fim da semana 0.** Critério de desempate: *qual você usaria todo dia*.
- [ ] Agendar no calendário: data-limite de launch público (dia 35 do M1 ≈ 11/ago/2026), revisão de go/no-go (dia 90 ≈ 05/out/2026).

**Gate: ✅ RESOLVIDO em 07/jul/2026 — EmbedMind venceu.** O Enclave volta ao banco de ideias; este roadmap é o plano ativo dos 90 dias. (Se o Enclave vencesse, este roadmap seria arquivado.)

---

## M1 — Semanas 1–4: o núcleo mínimo que já impressiona

Repo **privado** até o fim do marco. Prioridade absoluta: crash-safety antes de features.

| # | Entrega | Depende de | Status |
|---|---|---|---|
| 1.1 | Formato de arquivo único + **WAL/crash-safety** básica | — (fundação de tudo) | ✅ |
| 1.2 | KV store + API Rust limpa (`embedmind-core`) | 1.1 | ✅ |
| 1.3 | Busca vetorial **HNSW** in-file + embeddings ONNX embarcados (CPU) | 1.1, 1.2 | ✅ (incl. chunking de memórias longas) |
| 1.4 | Servidor MCP: `remember` / `recall` / `forget` | 1.2, 1.3 | ✅ (ADR 0009: stdio direto, sem SDK) |
| 1.5 | Memória automática de contexto de projeto | 1.4 | ✅ (raiz git / `.embedmind.toml`) |
| 1.6 | Instalação em 1 comando (`cargo install` + binários); testado com Claude Code **+ 1 outro agente** | 1.4 | 🔶 CLI completo (`serve` = servidor MCP) + pipeline de release por tag `v*` (3 plataformas, teto de tamanho); faltam `cargo publish` (A2) + teste manual com 2 agentes |
| 1.7 | README final: GIF de demo em 30s + **benchmark honesto** vs. sqlite-vec e zvec | 1.3, 1.6 | 🔶 README final + harness + benchmark honesto ✅; falta só o GIF/2º agente/`cargo publish`/GitHub Release ([MANUAL — founder]) |
| 1.8 | Testes de crash-recovery + fuzzing do formato no CI | 1.1 | ✅ |

**🎯 Milestone:** v0.1 funcional de ponta a ponta, **dogfooding diário do founder a partir da semana 2**.

---

## M2 — Semanas 5–8: lançamento público e primeiro ciclo de feedback

| # | Entrega | Depende de | Status |
|---|---|---|---|
| 2.1 | **Dia 35: repo público + lançamento coordenado** — Show HN, r/ClaudeAI, r/LocalLLaMA, r/rust, X. Post: *"I built persistent memory for coding agents in Rust — single file, no server"* | M1 completo | ⬜ [MANUAL — founder], previsto ≈ 11/ago/2026 |
| 2.2 | Toda issue respondida em <24h (capacidade de resposta É o marketing nesta fase) | 2.1 | ⬜ inicia pós-launch |
| 2.3 | **Full-text search** na engine | 1.2 | ✅ BM25 + fusão RRF (k=60) com o ranking vetorial — `crates/embedmind-core` (S9), `cargo test -p embedmind-core` |
| 2.4 | **Filtros de metadados** no `recall` | 2.3 | ✅ `Query::filters` (igualdade + faixa numérica), MCP e CLI — `crates/embedmind-core` (S10), `cargo test -p embedmind-core filters` |
| 2.5 | **Bindings Python** (destrava LangChain/agentes custom — multiplicador de audiência) | API da engine estável (M1) | ✅ `bindings/python` (PyO3 + maturin): `remember`/`recall`/`forget`/`stats`, mesmos arquivos `.mind`; suite pytest (`tests/test_e2e.py`, `tests/test_roundtrip.py`). Ainda NÃO publicado no PyPI; não cobre grafo (`related`, `entities`/`relations`, `supersedes`) — TypeScript segue pendente |
| 2.6 | 2º post técnico: a engine por dentro (WAL, HNSW em arquivo único) | 2.1 | ⬜ [MANUAL — founder], pós-launch |
| 2.7 | Releases quinzenais guiados pelas issues mais pedidas | 2.2 | ⬜ inicia pós-launch |

**🎯 Milestone:** projeto vivo em público, ciclo de release estabelecido, primeiros usuários externos reais.

---

## M3 — Semanas 9–12: aprofundar o núcleo

| # | Entrega | Depende de | Status |
|---|---|---|---|
| 3.1 | **Camada de grafo simples** (entidades + relações entre memórias) — o diferencial vs. "só vetor" que nenhum embarcado tem completo | 2.3, 2.4 | ✅ entidades/relações tipadas persistidas, `related(id \| entity)`, `recall --expand-related` — `crates/embedmind-core` + `embedmind-mcp`/`embedmind-cli` (S13), `cargo test -p embedmind-core graph` |
| 3.2 | **Proveniência básica** por memória (qual agente/sessão gravou) | 1.4 | ✅ `recall` devolve `provenance`, filtro `Query::agent`, `stats` com breakdown por agente — `crates/embedmind-core` (S14/C2), MCP e CLI |
| 3.3 | 3º post: caso de uso real com números ("30 dias usando memória persistente no meu fluxo com agentes") | dogfooding contínuo | ⬜ [MANUAL — founder], pós-launch |
| 3.4 | **Avaliação go/no-go do dia 90** | métricas abaixo | ⬜ previsto ≈ 05/out/2026 |

**🎯 Milestone:** núcleo diferenciado (vetor + texto + grafo), dados para a decisão de 90 dias.

### Métricas do go/no-go (dia 90, ~7 semanas pós-launch)

| Métrica | 🔴 Fraco | 🟡 Bom | 🟢 Forte | O que mede |
|---|---|---|---|---|
| Estrelas | < 300 | 300–1.500 | > 1.500 | Ressonância da mensagem (vaidade útil) |
| Issues/discussões de terceiros | < 10 | 10–40 | > 40 | **Uso real** |
| PRs externos aceitos | 0 | 1–5 | > 5 | Comunidade nascendo |
| Downloads recorrentes/semana | < 100 | 100–1.000 | > 1.000 | Retenção |

**Regras de decisão (compromisso prévio):**
- **2+ colunas 🟢** (sendo uma delas *issues*) → **GO**: seguir para M4–M6.
- **Maioria 🟡** → mais 90 dias no núcleo OSS com *um* reposicionamento de mensagem.
- **Maioria 🔴** com launch bem executado → **reempacotar** (mesma engine, outra porta de entrada). Só após 2 empacotamentos fracos a tese se considera refutada.
- **Cláusula anti-armadilha-do-construtor:** repo ainda privado no dia 45 = alarme vermelho; lançar o que existir.

---

## Fase FR — Frescor do conhecimento + observabilidade (pré-launch, decisão do founder 10/jul/2026)

Inserida ANTES do dia 35 (2.1), fora da ordem M1→M2→M3: origem no dogfooding via Painel
Agêntico, que passou a usar o próprio EmbedMind como memória do agente que o desenvolve.
Três achados de produto motivaram a decisão: (1) o ranking do `recall` não tinha
componente temporal — memória defasada semanticamente próxima vencia a correção mais
nova; (2) as relações `contradicts`/`refines` registravam o conflito mas o `recall` não
agia sobre elas; (3) zero observabilidade de operações (nenhum log estruturado), e o lock
exclusivo do arquivo impedia inspeção concorrente. "Conhecimento versionado" é
diferencial de anúncio que nenhum store embarcado tem — por isso entra antes do launch,
não depois. Detalhe das stories: [docs/01-spec.md](docs/01-spec.md) §"FR", breakdown de
tarefas: [docs/03-tasks.md](docs/03-tasks.md) "Fase FR".

| # | Entrega | Depende de | Status |
|---|---|---|---|
| FR0 | Docs (README/ROADMAP) ao estado real do código | — | ✅ (esta atualização e refresh subsequentes) |
| FR1 | `supersedes` — conhecimento versionado de primeira classe (S19) | grafo (3.1) | ✅ flag no record + aresta de grafo na mesma transação ([ADR 0013](docs/adr/0013-supersedes-flag-no-record.md)); `cargo test -p embedmind-core supersede` + `crash_supersede.rs` |
| FR2 | Recência na fusão do `recall` (S20) | S9 (RRF) | ✅ terceira lista na fusão RRF (`created_at` desc.) ([ADR 0014](docs/adr/0014-recencia-terceira-lista-rrf.md)) |
| FR3 | Curadoria na escrita — near-duplicates no `remember` (S21) | FR1 | ✅ aviso de near-duplicate na escrita, só considera memórias vivas/não-superseded do mesmo agente |
| FR4 | Op-log estruturado no `serve` (S22) | — | ✅ `serve --op-log <path>` grava JSONL por chamada (latência, args, ids/scores) |
| FR5 | Relatório de uso — `embedmind report` (S23) | FR4 | ✅ `embedmind report [--op-log <path>] [--since N] [--json]` — agrega o op-log com o store (top recalled, nunca recalled na janela); degrada a totais do store sem op-log |

**🎯 Fase FR fechada.** As cinco entregas (`supersedes`, recência, curadoria de
near-duplicates, op-log estruturado e `report`) estão em `main` com teste — "conhecimento
versionado" (FR1+FR2+FR3) é o diferencial de anúncio do launch.

---

## Fase FT — Fechar as dívidas do NFR de recall/latência/RSS a 100k (pré-launch, decisão do founder 11/jul/2026)

Aberta pelo NFR reprovado da BQ1 (`ef_search` escalonado): `recall` p99 @ 100k medido em
1.224,62 ms contra o teto de 50 ms (24x acima), recall de pior-caso e RSS de pico também
fora do alvo na mesma medição. Três frentes independentes, detalhe em
[docs/03-tasks.md](docs/03-tasks.md) "Fase FT" e [ADR 0017](docs/adr/0017-otimizacao-do-full-text-escopo-e-metodo.md).

| # | Entrega | Status |
|---|---|---|
| FT1 | Profiling do meio full-text @ 100k (S24) | ✅ causa dominante identificada: a closure `keep` (recarga do registro por candidato), 88,8% do tempo — não I/O de página, não hashing |
| FT2 | Early termination no scan de BM25 (S25) | ✅ [ADR 0018](docs/adr/0018-early-termination-no-scan-bm25.md) — corte quando o upper bound do próximo candidato fica abaixo do k-ésimo score exato; resultado byte-idêntico |
| FT3 | Compressão delta+varint + skip lists nas postings (S26) | ✅ `format_version` 4 (delta+varint, [ADR 0021](docs/adr/0021-postings-fts-delta-varint.md)) e 5 (skip lists, [ADR 0022](docs/adr/0022-postings-fts-skip-lists.md)); formato aditivo, arquivo antigo continua legível |
| FT4 | Recall de pior-caso @ 100k (S27) | ✅ [ADR 0019](docs/adr/0019-recall-tie-aware-no-harness.md) — a cauda era artefato de grading (empates de score em texto duplicado), não miss do HNSW; grading virou tie-aware |
| FT5 | Estouro de RSS de pico @ 100k (S28) | ✅ [ADR 0020](docs/adr/0020-rss-de-pico-era-o-harness-nao-o-engine.md) — causa era o harness (baseline brute-force retido além do uso), não a engine; RSS caiu para ~118 MiB |

**Fechamento da fase (13/jul/2026):** recall@10 (tie-aware) e RSS de pico **aprovados** a
100k. `recall p99 @ 100k` **reprovado**: 255,12 ms medido contra o teto de 50 ms — caiu
~4,8x com FT2+FT3 (de 1.224,62 ms), mas não fechou (ver ADR 0017 "Fechamento da fase FT").

**FT6 — o benefício do full-text (13/jul/2026):** mediu pela primeira vez o que o full-text
*compra*, não só o que custa — 100 queries lexicais (identificadores, flags CLI, erros
literais, hashes, ULIDs) comparando `recall` híbrido vs. `recall_vector` puro. Lift de
recall@10: **+0,09 @10k → +0,18 @100k** — dobra com o corpus, não encolhe (o vetor-puro
degrada de 0,9100 para 0,8200 com mais colisão vetorial; o híbrido segura 1,0000 nos dois).
Detalhe em [ADR 0017](docs/adr/0017-otimizacao-do-full-text-escopo-e-metodo.md) §"O
benefício do full-text".

## Fase BMW — BlockMax-WAND para fechar o NFR de latência (decisão do founder 13/jul/2026)

Com o lift medido em mãos (FT6, crescente com o corpus), o founder decidiu manter o
full-text como default e investir na reescrita BlockMax-WAND em vez de tornar o full-text
opt-in — decisão completa, com critério de reversão, em
[ADR 0023](docs/adr/0023-blockmax-wand-decisao-fase-bmw.md).

| # | Entrega | Status |
|---|---|---|
| BMW1 | Bound de impacto por bloco no skip index — `format_version` 6 (ADR 0024): cada skip entry ganha `last_id` (block max doc id) ao lado de `max_term_freq`, o par `(block_max_docid, block_max_impact)` que o BMW pula um bloco por. Só formato + bound; a passada 1 segue linear | ✅ ENTREGUE (fv6, round-trip v4/v5/v6 + fuzz/crash verdes; `min(doc_len)` avaliado e rejeitado — não persistível) |
| BMW2 | Reescrita da passada 1 de `fts::search` em BlockMax-WAND sobre o bound do fv6 (ADR 0025): busca DAAT com pivô WAND + refinamento block-max, threshold = k-ésimo score exato do heap, desempate por `record_id` antes do corte nos dois caminhos, comparações de bound em f64 com folga anti-arredondamento. Caminho linear preservado como oráculo (`search_linear`) e como produção para arquivos v4/v5 | ✅ ENTREGUE (suite de equivalência tripla — corpus determinístico + fronteira de empates + proptest — verde; contadores de blocos pulados/avaliados prontos para a BMW3) |
| BMW3 | Medição @ 10k e @ 100k pelo harness oficial (`benches/run_all.sh --full`), decidindo se `recall p99 @ 100k < 50 ms` passa | ✅ ENTREGUE — **reprovado**: 224,00 ms @ 100k (praticamente idêntico ao patamar pré-BMW). Causa raiz medida (`benches/src/bin/bmw_reach.rs`): BMW ativa em 82,8% das queries, mas só 0,05% dos blocos tocados são de fato pulados sem decodificar — o corpus sintético não tem a concentração de postings que o BMW foi desenhado para explorar |
| BMW4 | Fechamento: atualizar ADR 0017/0025, README e ROADMAP com o resultado, qualquer que seja | ✅ ENTREGUE (nesta task) |

**Critério de reversão avaliado, decisão pendente:** o BMW não fechou o NFR (224,00 ms vs. teto de
50 ms). O critério de reversão do ADR 0023 (aceitar a limitação de latência documentada vs.
reverter full-text para opt-in) está em aberto — decisão do founder, não tomada nesta task.
Veredito completo e causa raiz em
[ADR 0017](docs/adr/0017-otimizacao-do-full-text-escopo-e-metodo.md) §"Fechamento da fase BMW" e
[ADR 0025](docs/adr/0025-blockmax-wand-na-busca-fts.md) §"BMW-3".

## Fase FTOPT — atacar o custo real do `keep` (pós-BMW, 13/jul/2026)

Com o BMW reprovado por falta de blocos puláveis, o alvo voltou ao custo dominante medido na FT1:
a recarga do registro completo por candidato na closure `keep` (88,8% do tempo @100k, ADR 0017).

| # | Task | Status |
|---|---|---|
| FTOPT-0 | Profiling confirmatório do `keep` (breakdown aceito × rejeitado, `KeepOutcome`) @10k e @100k | ✅ ENTREGUE — 97,9% aceitos @10k, **99,9% @100k**: "pular I/O nos rejeitados" tem teto ~0,1% (ADR 0017 §FTOPT-0) |
| FTOPT-1 | Filter-meta sidecar (`format_version` 7, [ADR 0027](docs/adr/0027-filter-meta-sidecar-fv7.md)): `record_id → (flags, project, agent, doc_len)` fora do registro, escrito na mesma transação; `keep`/`doc_len` decidem sem tocar o B-tree para aceitos E rejeitados; registro completo só para top-k e filtros custom. Redesenho imposto pelo dado da FTOPT-0 | ✅ ENTREGUE (equivalência vs. oráculo fv6, crash sweep, fuzz; ganho @100k **não medido** — fica para a task de fechamento) |
| FTOPT-2 | `doc_len` pré-computado (elimina a 2ª recarga da normalização BM25, 4,5% no FT1). Reabre o trade-off do [ADR 0011](docs/adr/0011-fulltext-index.md) (que rejeitou persistir `doc_len`): o sidecar da FTOPT-1 já acomoda o campo sem custo estrutural | ✅ ENTREGUE — absorvida pela FTOPT-1: o campo `doc_len` já entrou no sidecar fv7 e as closures BM25 já leem dele. Esta task fechou as pontas do critério de pronto: teste **positivo** (score muda com `doc_len` do sidecar corrompido ⇒ leitura vem do sidecar, não do conteúdo) + teste **negativo** de invariante (divergência é erro tipado). Sem benchmark @100k (FTOPT-4) |
| FTOPT-4 | Medição @100k pelo harness oficial (`profile_recall`) e veredito do NFR `recall p99` — decisão de produto do founder sobre o resultado | ✅ ENTREGUE — 224,00 ms pós-sidecar fv7 (baseline da fase, ADR 0017 §FTOPT-4) |
| FTOPT-5 | Profiling confirmatório pós-sidecar do caminho BMW real (`search_bmw_profiled`) | ✅ ENTREGUE — novo gargalo: decode de blocos de postings 32,7% + vetor/HNSW 36,0%; bound WAND/block-max só 11,4% (ADR 0017 §FTOPT-5) |
| FTOPT-6 | Testar a hipótese de alocação por bloco em `decode_block` (reuso de `Vec` entre blocos) | ✅ ENTREGUE — mudança aplicada (estritamente não-pior), mas ganho não mensurável (25,7%→32,8%, dentro do ruído); alocação não era o custo dominante (ADR 0017 §FTOPT-6) |
| FTOPT-7 | Profiling granular do interior de `decode_block`: laço varint vs. revalidações | ✅ ENTREGUE — confirmado: o laço `decode_delta_run` domina (59,9% da fase de decode), revalidação irrelevante (0,6%); gargalo é o formato de postings em si, sem mudança local acionável (ADR 0017 §FTOPT-7) |
| FTOPT-8 | Novo formato de postings frame-of-reference (`format_version` 8, [ADR 0028](docs/adr/0028-postings-fts-frame-of-reference.md)) para eliminar o laço varint identificado pela FTOPT-7 | ✅ ENTREGUE — laço varint -70,5%, `recall` p99 @100k 266,03 ms → **133,65 ms** (ADR 0017 §FTOPT-8) |

**Fase FTOPT fechada em 14/jul/2026** (founder, ADR 0017 §"Recalibração final do NFR e fechamento
da fase"): trajetória completa 224,00 ms → 133,65 ms p99 @100k (**-40%**). O NFR de latência foi
recalibrado duas vezes no mesmo dia (50 ms → 100 ms → 150 ms, honestamente registrado, não
enterrado — decisões PR #40 e #41) porque o gargalo deixou de ser predominantemente full-text
(vetor + WAND/bound já somam mais da metade do tempo pós-FTOPT-8); o patamar final de 133,65 ms
**passa** no NFR revisado de 150 ms — a fase encerra com o NFR atendido, não reprovado.

---

## Pós-90 dias (M4–M6, condicionado a GO)

| Frente | Conteúdo | Depende de |
|---|---|---|
| **Vitrine da engine** | App pequeno de notas/memória por voz 100% local (Chefe de Gabinete em miniatura) — demonstra a engine ao público não-dev | Engine estável, 3.1 |
| **Bindings adicionais** | TypeScript; Swift/C conforme demanda | 2.5 |
| **Extensões além do núcleo** | Time-travel/histórico, criptografia at-rest, RBAC/auditoria, sync de equipe — o que priorizar (e como empacotar) é decisão do founder pós-tração, guiada pelas issues mais pedidas | GO confirmado |
| **Segunda aposta** | Avaliar tirar o **Paperjet** do banco | GO confirmado + apetite |
| **Ingestão de código como fonte de memória** | Indexar localmente (Tree-sitter, sem LLM externo) código/docs do projeto como memórias além das gravadas via `remember` — mesma promessa "nada sai da máquina", mas cobrindo conhecimento *já existente*, não só o acumulado em sessão. Avaliar após GO; risco de virar concorrência direta de players como o Graphify (YC, tração grande) em vez de nicho adjacente | Engine estável, 3.1 (grafo) |

**Fallback de demanda** (se as apostas frustrarem): CLI de relatório de desperdício de tokens (fatia mínima da Torre de Controle) — valida em 1 semana.

---

## Grafo de dependências (funcionalidades)

```
WAL/arquivo único (1.1)
 ├── KV + API Rust (1.2)
 │    ├── HNSW/vetor (1.3) ──┐
 │    └── Full-text (2.3)    ├── MCP remember/recall/forget (1.4)
 │         └── Filtros (2.4) ┘    ├── Contexto de projeto (1.5)
 │              └── Grafo (3.1)   └── Proveniência básica (3.2)
 ├── Fuzzing/crash tests (1.8) — contínuo
 └── Criptografia at-rest (reservada no formato; M4+, se GO)
API estável ── Bindings Python (2.5) ── Bindings TS/Swift (M4+)
```

## Riscos que moldam a sequência

- **Commoditização** (sqlite-vec/LanceDB/zvec com times pagos) → por isso M1 = 4 semanas até usável, e o posicionamento é "memória para agentes", não "database para RAG".
- **MCP perder relevância** → engine em camadas; a casca MCP é substituível.
- **Corrupção de dados** (mata o moat de confiabilidade) → WAL antes de features, fuzzing no CI, honestidade brutal no changelog.
- **Burnout OSS** → SLA "best effort" público, releases em ritmo fixo, feature grande só com 2+ pedidos.
