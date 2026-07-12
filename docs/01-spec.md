# Spec comportamental — EmbedMind

> Documento canônico do pacote SDD (01 de 04). Fonte de verdade do **o quê** — comporta-
> mento observável, nunca organização de código. Cada acceptance criterion é verificável
> por comando ou teste. Escrito em 08/jul/2026 contra o estado real do repo: stories
> marcadas **[✅ implementada]** já têm código e testes em `main` — o agente NÃO deve
> reimplementá-las, apenas mantê-las verdes. Specs normativas complementares:
> [FORMAT.md](FORMAT.md) (formato `.mind` byte a byte) e [TESTING.md](TESTING.md)
> (harness de crash, fuzzing, invariantes I1–I5).

## Convenções desta spec

- Ator "agente" = host MCP conectado (Claude Code, Cursor, custom); ator "usuário" =
  humano no CLI.
- Toda latência citada assume hardware comum, CPU-only, arquivo com 100k memórias,
  salvo indicação.
- Erros são **tipados e nunca derrubam o processo servidor**: falha de engine durante
  tool call vira resultado com `isError: true`; erro de protocolo vira código JSON-RPC.

---

## P0 — núcleo v0.1 (M1)

### S1. Lembrar uma memória — `remember` [✅ implementada]

Como agente, quero gravar um fato com metadados para reencontrá-lo semanticamente em
sessões futuras.

- **Dado** um arquivo `.mind` aberto, **quando** chamo `remember(content, metadata?,
  project?)`, **então** recebo `{id, project}` com id ULID (ordenável por tempo) e a
  memória está imediatamente visível para `recall` e durável (fsync no commit).
- **Dado** `project` omitido, **quando** gravo, **então** o projeto detectado do cwd é
  estampado; `project: null` explícito grava memória global.
- **Dado** conteúdo > 510 tokens, **quando** gravo, **então** o texto é indexado em
  janelas de 510 tokens com overlap de 64; o registro permanece inteiro e único;
  `recall` devolve a memória completa, nunca um chunk.
- **Borda:** conteúdo > 128 chunks (~57k tokens) → erro tipado, nada é indexado
  parcialmente. Conteúdo vazio → erro tipado. Metadado com valor não-escalar → erro
  tipado.
- **Borda:** proveniência básica (agent, session_id?, created_at) é estampada em toda
  memória; via MCP o `clientInfo.name` do handshake vira o agente.
- **Verificação:** `cargo test -p embedmind-core api::` e teste E2E do CLI.

### S2. Buscar semanticamente — `recall` [✅ implementada]

Como agente, quero buscar por significado (não keyword) e receber os melhores hits com
score, escopados ao projeto em que estou.

- **Dado** memórias gravadas, **quando** chamo `recall(query, limit?=8, project?,
  scope?)`, **então** recebo `hits: [{id, content, score, project, provenance,
  created_at_micros}]` ordenados por similaridade (coseno), melhor primeiro, e o escopo
  efetivamente aplicado é ecoado na resposta.
- **Dado** que estou num diretório de projeto, **quando** chamo sem `project`/`scope`,
  **então** só memórias do projeto detectado (+ nenhuma global de outro projeto)
  retornam; `scope: "all"` é a saída explícita para busca global.
- **Dado** memórias tombstoned (esquecidas), **quando** busco, **então** elas nunca
  aparecem — o status é re-verificado contra o registro no momento da busca, nunca
  confiado ao grafo.
- **Borda:** filtro deixa resultado incompleto → `ef_search` cresce adaptativamente
  (×4 por rodada, teto = grafo inteiro) — degrada rumo a scan honesto, nunca
  sub-retorna em silêncio.
- **Borda:** arquivo vazio → `hits: []`, não erro. `limit: 0` → lista vazia.
- **NFR:** p99 < 50 ms com 100k memórias (CPU-only, cache quente).
- **Verificação:** `cargo test -p embedmind-core recall` + property tests vs. modelo
  de referência linear.

### S3. Esquecer — `forget` [✅ implementada]

Como agente, quero apagar uma memória específica por id.

- **Dado** um id existente, **quando** chamo `forget(id)`, **então** recebo
  `{count: 1}` e a memória some de todo `recall` subsequente (soft-delete/tombstone;
  espaço é recuperado pelo `vacuum` offline).
- **Borda:** id inexistente ou já esquecido → `{count: 0}`, idempotente, zero bytes
  escritos. Id malformado → erro tipado.
- **Futuro (fora da v0.1):** `forget` por query/idade exigirá `confirm: true`
  obrigatório — deleção em massa não pode ser acidente de um tool call.
- **Verificação:** `cargo test -p embedmind-core forget`.

### S4. Durabilidade e integridade (a promessa da marca) [✅ implementada]

Como usuário, confio que nenhuma memória confirmada se perde e que o arquivo nunca fica
irrecuperável — mesmo com `kill -9` ou queda de energia no meio da escrita.

- **Dado** um crash em QUALQUER ponto de I/O (antes/depois de cada fsync, escrita
  parcial de página, torn write em granularidade de setor), **quando** reabro o
  arquivo, **então** o recovery roda automático e silencioso: toda transação com commit
  válido está presente (I-durabilidade), nenhuma meia-transação é visível
  (I-atomicidade), todos os checksums batem (I-integridade).
- **Dado** um WAL com cauda torta, **quando** abro, **então** o prefixo commitado é
  aplicado e a cauda inválida descartada sem intervenção.
- **Dado** hardware que mente no fsync, **então** integridade se mantém sempre;
  durabilidade dos últimos commits pode se perder (mesma postura do SQLite,
  documentada).
- **Borda:** segundo processo escritor no mesmo arquivo → erro claro imediato
  (lock advisory; `LockFileEx` no Windows); leitores concorrentes permitidos.
- **Verificação:** `cargo test --workspace` (harness `tests/crash.rs` +
  `tests/crash_records.rs` roda a varredura completa de pontos de injeção; falha
  imprime tupla reproduzível).

### S5. Arquivo hostil ou incompatível nunca derruba nem confunde [✅ implementada]

- **Dado** um arquivo corrompido/adversarial, **quando** abro ou decodifico qualquer
  página/record/WAL, **então** recebo erro tipado — nunca panic, nunca alocação
  desmedida (todo length-prefix é validado antes de alocar).
- **Dado** `format_version` maior que o suportado, **então** recusa com mensagem clara
  (política G4); flag `encrypted` ligada → recusa tipada (criptografia é feature
  futura, reservada no formato).
- **Dado** um arquivo gravado com outro modelo de embedding (model_id/dims diferentes
  no header), **quando** abro, **então** recusa tipada — misturar embeddings é erro;
  o caminho é `embedmind reembed` (futuro).
- **Verificação:** 5 alvos de fuzzing (`fuzz_header`, `fuzz_page`, `fuzz_record`,
  `fuzz_wal_replay`, `fuzz_open_full`) verdes; corpos também rodam como smoke tests em
  `cargo test` em toda plataforma.

### S6. Servidor MCP plug-and-play [✅ implementada]

Como usuário, adiciono uma linha no meu agente e ele ganha memória.

- **Dado** `claude mcp add embedmind -- embedmind serve --file <path>`, **quando** o
  host inicia a sessão, **então** o handshake `initialize`/`ping`/`tools/list` responde
  com as 3 tools de schema estável; stdout é canal exclusivo do protocolo (logs em
  stderr).
- **Dado** o cwd do agente dentro de um repo git ou de uma pasta com `.embedmind.toml`
  (chave `project`), **então** o marcador mais próximo subindo a árvore define o
  projeto (toml vence git); `remember`/`recall` usam esse contexto por default.
- **Verificação:** teste E2E que dirige uma sessão MCP completa via pipes stdio contra
  o binário real.

### S7. CLI standalone [✅ implementada]

- **Dado** o binário instalado, **quando** rodo `embedmind remember|recall|forget|
  stats|vacuum|serve` (com `--file`, `--project`, `--global`, `--all`, `--limit`),
  **então** cada comando opera sobre `~/.embedmind/memory.mind` por default com a mesma
  semântica das tools MCP; `stats` reporta contagens, layout do arquivo, entradas de
  índice e o modelo de embedding gravado.
- **Borda:** `embedmind vacuum` reconstrói por cópia e imprime o tamanho antes → depois
  e a contagem recuperada (S11). Erros saem em stderr com exit code ≠ 0.
- **Verificação:** testes E2E do crate `embedmind-cli` sobre o binário real.

### S8. Instalação em 1 comando [◔ parcial — falta release]

- **Dado** `cargo install embedmind` OU download de binário pré-compilado do Releases,
  **então** um único binário funciona em Windows, Linux e macOS sem dependência externa
  (modelo ONNX + tokenizer embutidos; ONNX Runtime resolvido no build).
- **Dado** o mesmo arquivo `.mind` copiado entre plataformas, **então** abre idêntico
  (little-endian fixo).
- **Pendente:** publicação no crates.io + binários de release no GitHub + teste manual
  com Claude Code **e mais 1 agente** (hoje só o build por fonte funciona).
- **NFR:** artefato de release comprimido < 40 MB incluindo modelo (~20 MiB hoje; o
  binário nu linka ONNX Runtime estático e passa de 40 MB — ver [ADR 0010](adr/0010-teto-de-tamanho-governa-artefato-comprimido.md));
  RAM < 300 MB para 100k memórias.
- **Verificação:** CI de release produz artefatos para as 3 plataformas; smoke test de
  instalação documentado.

---

## P1 — M2 (lançamento + híbrido)

### S9. Busca híbrida full-text + vetor [✅ implementada]

- **Dado** memórias com termos exatos (nomes de função, siglas, ids), **quando** chamo
  `recall`, **então** a lista final funde ranking vetorial e BM25 por **Reciprocal Rank
  Fusion (k=60)** — um termo exato raro encontra a memória mesmo quando o embedding
  não aproxima.
- **Dado** só match vetorial ou só match textual, **então** o hit ainda aparece
  (a fusão nunca exige interseção).
- **Borda:** índice full-text ausente em arquivo antigo → recall degrada para
  vetor-só com aviso, nunca erro.
- **Verificação:** testes de fusão com casos de ouro (termo raro, sinônimo semântico,
  ambos) + property tests.

### S10. Filtros de metadados no recall [✅ implementada]

- **Dado** memórias com metadados tipados, **quando** chamo `recall(query,
  filters: {chave: valor|faixa})`, **então** só memórias que satisfazem TODOS os
  filtros retornam, com a mesma garantia anti-sub-retorno da S2.
- **Borda:** filtro por chave inexistente → 0 hits, não erro; tipo incompatível →
  erro tipado.
- **Verificação:** `cargo test -p embedmind-core filters`.

### S11. Vacuum — recuperar espaço [✅ implementada]

- **Dado** memórias esquecidas acumuladas, **quando** rodo `embedmind vacuum`,
  **então** tombstones e cadeias de overflow órfãs são removidos, o índice HNSW é
  reconstruído sem os mortos, e o arquivo resultante é menor ou igual; crash no meio
  do vacuum nunca perde o arquivo original (cópia, nunca in-place destrutivo).
- **Verificação:** teste de round-trip (ingest → forget 50% → vacuum → invariantes +
  tamanho reduzido) + crash test do vacuum.

### S12. Bindings Python [⬜ pendente]

- **Dado** `pip install embedmind`, **quando** uso `Store` do Python, **então**
  remember/recall/forget/stats funcionam com a mesma semântica (mesmos arquivos `.mind`
  compatíveis), destravando LangChain e agentes custom.
- **Verificação:** suite pytest dos bindings no CI.

---

## P2 — M3+ (profundidade e funil)

### S13. Camada de grafo — entidades e relações [✅ implementada]

- **Dado** memórias que mencionam entidades, **quando** gravo com
  `entities`/`relations` explícitas (extração automática NÃO é escopo desta story),
  **então** posso navegar `related(id | entity)` e o `recall` pode expandir 1 salto
  para puxar contexto conectado.
- **Borda:** relação para memória esquecida some junto do tombstone.
- **Verificação:** testes de grafo (inserção, navegação, expansão em recall).

### S14. Proveniência básica exposta [◔ dados já gravados; falta expor]

- **Dado** memórias gravadas por agentes distintos, **quando** consulto (`recall` já
  devolve `provenance`; `stats`/filtros por agente no M3), **então** vejo qual
  agente/sessão gravou o quê.
- **Verificação:** teste de round-trip de proveniência via MCP e CLI.

### S15. Não-regressão de performance [✅ guard no CI; contínua]

- **Dado** qualquer mudança na engine, **quando** o CI roda o harness de benchmark,
  **então** recall@10, latências p50/p99 (incl. cold-open), throughput de ingest,
  tamanho de arquivo e RSS de pico não regridem além do limiar; números publicados
  seguem [BENCHMARKS.md](BENCHMARKS.md) — incluindo onde perdemos para
  sqlite-vec/zvec.
- **Verificação:** `benches/run_all.sh` (com `BASELINE=<results.json>` para o guard
  §5) e o workflow `.github/workflows/bench.yml`, que roda em todo PR/push que toca
  engine ou harness e falha o job quando um limiar do §5 é cruzado.

### S16. Recall estável em escala — `ef_search` proporcional ao índice [✅ implementada]

Contexto medido (run 2026-07-09, `agent-mem-100k`): recall@10 médio caiu de 0,9953
(@10k) para 0,9313 e a pior query para **0,20** — causa raiz: `ef_search` default fixo
em 64 (`format.rs`) não escala com o tamanho do índice, e a adaptação existente só
dispara em sub-retorno por filtro, nunca por qualidade. Há folga de latência (p99
15,5 ms medido vs. teto de 50 ms) para gastar em recall.

- **Dado** um índice com 100k memórias, **quando** chamo `recall` com os defaults,
  **então** o `ef_search` efetivo cresce com o tamanho do índice (fórmula/patamares a
  decidir por sweep no harness e registrar em ADR — nunca outra constante fixa) e o
  recall@10 vs. brute-force fica ≥ 0,95 na média e ≥ 0,70 na pior query, com query
  p99 ainda < 50 ms (alvos propostos em 10/jul/2026 a partir do sweep; ajustar pelo
  dado medido, registrando o porquê).
- **Dado** um arquivo pequeno (ex: 1k memórias), **quando** chamo `recall`, **então**
  a latência não regride além do limiar do §5 do BENCHMARKS.md (o escalonamento não
  pode punir quem tem pouco dado).
- **Borda:** `Query::ef_search(n)` explícito continua soberano — o escalonamento só
  governa o default.
- **Verificação:** `benches/run_all.sh` nos dois datasets; harness passa a reportar
  também a distribuição do recall por query (mín/p10/p50), não só média e mínimo.

### S17. Benchmark decomposto e simétrico [⬜ pendente]

O cronômetro de query do EmbedMind mede embed da query + busca híbrida + carga dos
registros; os concorrentes recebem vetor pronto e devolvem ids. A tabela publicada
compara coisas diferentes sem rotular — corrigir a medição, não o marketing.

- **Dado** um run do harness, **quando** a tabela é emitida, **então** a latência do
  EmbedMind aparece decomposta (`embed` da query vs. `engine`: busca+fusão+carga), e
  existe uma seção **texto→resultado** onde cada concorrente paga o mesmo custo de
  embedding (mesmo modelo/pipeline ONNX, medido fora dele e somado) — simetria nos
  dois sentidos: index-only compara índice com índice; texto→resultado compara
  produto com produto.
- **Dado** um run, **então** `results/<versão>.json` e `latest.md` saem da MESMA
  invocação (hoje divergem: md diz "not measured", json tem números) e cada linha da
  tabela declara o escopo do sistema (o que devolve: ids vs. conteúdo completo; o que
  persiste: só vetores vs. texto+metadados+índices).
- **Verificação:** `benches/run_all.sh` + revisão da tabela gerada; teste do renderer
  cobrindo a decomposição.

### S18. Comparação com concorrente da categoria de produto [✅ implementada]

sqlite-vec/zvec são baselines de camada de índice; a alternativa que um dev de agente
realmente considera é um vector store local que também embeda (Chroma em modo
local/embedded, com o mesmo all-MiniLM-L6-v2, é a briga justa).

- **Dado** o harness com a feature de comparação habilitada, **quando** rodo a suite,
  **então** o Chroma (modo local, versão pinada) aparece na seção texto→resultado com
  recall@10, p50/p99 e tamanho em disco, sob as mesmas regras de honestidade (S15);
  toolchain ausente reporta "not measured", nunca número inventado.
- **Verificação:** `benches/run_all.sh` com a feature ligada num ambiente com Python
  disponível (dependência externa do founder, como as toolchains de sqlite-vec/zvec).

Implementada: `--features compare-chroma`, adapter em `benches/src/competitors.rs`
(`run_chroma`) driblando `benches/chroma_bench.py` via subprocess JSON (sem rede/servidor),
Chroma pinado a `chromadb==1.5.9`. Recebe os mesmos vetores pré-computados que
sqlite-vec/zvec (nunca reembeda); recall@10 calculado do lado Rust contra o baseline
brute-force compartilhado. Verificado com `benches/run_all.sh agent-mem-10k` +
`COMPARE="--features compare-chroma"`.

---

## P1 — FR: frescor do conhecimento + observabilidade (pré-launch — decisão do founder 2026-07-10)

> Origem: dogfooding via Painel Agêntico (10/jul/2026), que passou a usar o EmbedMind
> como memória do agente que desenvolve este repo. Três achados de produto: (1) o
> ranking não tem componente temporal — memória defasada semanticamente próxima vence
> a correção mais nova; (2) relações `contradicts`/`refines` registram o conflito mas
> o recall não age sobre elas; (3) zero observabilidade de operações (nenhum log
> estruturado) e o lock exclusivo impede inspeção concorrente do arquivo. Decisão:
> entra ANTES do launch de 11/ago — "conhecimento versionado" é diferencial de anúncio
> que nenhum store embarcado tem.

### S19. `supersedes` — conhecimento versionado de primeira classe [✅ implementada]

Como agente, quero corrigir um fato gravado sem perder o histórico: a memória nova
substitui a antiga no recall, mas a antiga continua navegável como versão anterior.

- **Dado** `remember(content, supersedes: [id_A])`, **quando** gravo a memória B,
  **então** A deixa de aparecer em QUALQUER `recall` subsequente (status re-verificado
  contra o registro no momento da busca, mesma regra dos tombstones da S2), mas
  `get(A)` continua funcionando e `related(B)` mostra a aresta `supersedes → A`
  (com a direção inversa visível em `related(A)`).
- **Dado** uma cadeia A←B←C (C supersedes B, que supersedes A), **então** só C aparece
  no recall; a cadeia é navegável passo a passo via `related`.
- **Borda:** alvo inexistente ou tombstoned → erro tipado (mesma regra das relations);
  alvo de OUTRO projeto → erro tipado (não cruzar escopo). `forget` de B NÃO
  ressuscita A (a exclusão de A é estado próprio, não derivada por travessia de grafo
  — default a registrar em ADR com a alternativa rejeitada e o porquê).
- **Borda:** `vacuum` PRESERVA memórias superseded (são histórico, não lixo);
  `forget` explícito delas continua possível.
- **Formato:** sem quebra — reusar a infra de relações tipadas do grafo (S13);
  representação exata (flag no record vs. índice de exclusão) decidida em ADR;
  FORMAT.md atualizado se houver página/campo novo (versão aditiva, política G4).
- **Cascas:** MCP `remember` ganha `supersedes: [ids]` (retrocompatível); CLI
  `remember --supersedes ID` (repetível).
- **Verificação:** `cargo test -p embedmind-core supersede` + testes de protocolo MCP
  + E2E CLI; crash test se tocar formato.

### S20. Recência na fusão do recall [✅ implementada]

Como agente, quero que empate semântico penda para o conhecimento mais novo — sem que
um match forte antigo seja derrubado por novidade irrelevante.

- **Dado** memórias relevantes de idades distintas, **quando** `recall` roda,
  **então** uma terceira lista entra na fusão RRF k=60 ao lado de vetor e texto: os
  MESMOS candidatos das buscas de conteúdo (união vetor+texto), reordenados por
  `created_at` decrescente — recência desempata entre o que JÁ é relevante, nunca
  traz item irrelevante só por ser novo. Só posições de rank, nunca escalas
  (ADR 0005 preservado: nada a calibrar).
- **Dado** um match forte antigo (rank 0 em vetor e texto), **então** ele só perde
  para outro hit de conteúdo comparável — a lista de recência sozinha nunca inverte
  um domínio de conteúdo (propriedade do RRF: a contribuição máxima de uma lista é
  `1/(k+1)`).
- **Default vs. opt-in:** decidir POR MEDIÇÃO no harness — se o recall@10 vs.
  brute-force regredir além do limiar do BENCHMARKS.md §5 com recência ligada, ela
  vira opt-in (`recency: true` em Query/MCP/CLI); registrar a decisão e os números
  em ADR.
- **Borda:** a lista de recência respeita os mesmos filtros (escopo, tombstone,
  superseded, metadados, agente) das demais — nunca reintroduz um excluído.
- **Verificação:** casos de ouro (fato + correção: a correção vem primeiro; match
  forte antigo vs. novo fraco: o antigo segue primeiro) + property tests da fusão de
  3 listas + `benches/run_all.sh` antes/depois.

### S21. Curadoria na escrita — near-duplicates no `remember` [✅ implementada]

Como agente, quero saber NA HORA DE GRAVAR que já existe memória parecida, para
decidir forget/supersedes/manter — higiene de conflito onde há contexto para julgar.

- **Dado** `remember` de conteúdo similar a memórias existentes, **então** a resposta
  inclui, além de `{id, project}`, `similar: [{id, content (truncado), score,
  created_at_micros}]` acima de um limiar de similaridade (valor decidido por medição
  no corpus do harness, registrado em ADR); lista vazia quando não há parecidos.
  A gravação SEMPRE acontece — informar, nunca bloquear.
- **Custo:** reusa o embedding já computado pelo próprio `remember` (zero embedding
  extra); NFR `remember` p99 < 200 ms fim a fim se mantém.
- **Borda:** near-duplicates só consideram memórias vivas, não-superseded e do MESMO
  escopo aplicado; primeira memória do arquivo → `similar: []`.
- **Cascas:** MCP retorna o campo novo (retrocompatível); CLI imprime aviso legível
  ("memória parecida existente: <id> — <trecho>").
- **Verificação:** testes core (limiar, escopo, truncamento) + protocolo MCP + E2E
  CLI + benchmark do `remember` (p99 dentro do NFR).

### S22. Op-log estruturado do servidor [✅ implementada]

Como operador (painel/founder), quero observar o que o agente grava e busca — sem
tocar no arquivo `.mind` (lock exclusivo) e sem poluir o protocolo.

- **Dado** `embedmind serve --op-log <path>`, **então** cada tool call appenda 1 linha
  JSON (JSONL) com `{ts, tool, args resumidos (query/content truncados ~200 chars),
  ids retornados, scores, latency_ms, project, isError}`.
- **Dado** a flag ausente, **então** zero custo e nenhum arquivo criado.
- **Borda:** falha de escrita no op-log NUNCA falha a tool call (aviso em stderr,
  resposta normal); stdout permanece canal exclusivo do protocolo (S6).
- **Borda:** arquivo é append-only e cada linha é JSON independente — um leitor pode
  tail-ar a partir de qualquer ponto (consumidor imediato: card de memória do Painel
  Agêntico, tail via SSE).
- **Verificação:** E2E MCP dirigindo sessão com `--op-log` e validando linhas
  parseáveis uma a uma, incluindo caso de erro de engine (`isError: true` logado).

### S23. Relatório de uso — `embedmind report` [✅ implementada]

Como usuário do EmbedMind, quero saber se a memória está sendo USADA — quantas
sessões conectaram, quanto os agentes buscaram, quais memórias são reaproveitadas
e quais nunca foram servidas — para confiar no valor do produto.

- **Dado** `embedmind report --op-log <path> [--since N]`, **então** imprime a
  agregação da janela: sessões, recalls (vazios/erros/latência p50-p99), remembers,
  forgets, top memórias recalladas (contador por memória + preview) e "nunca
  recalladas na janela" (peso morto) sobre as memórias vivas.
- **Dado** `--json`, **então** a MESMA agregação sai como um objeto JSON — primeira
  saída estruturada do CLI (consumidor imediato: card de memória do Painel Agêntico).
- **Dado** op-log ausente/inexistente, **então** degrada para totais do arquivo com
  instrução de como capturar uso — nunca erro.
- **Decisão de design — contadores derivados, não estado:** os contadores por
  memória vêm da agregação do op-log, NÃO de colunas no record (formato do .mind
  intocado; `recall` continua leitura pura). O `initialize` do serve appenda a linha
  `{tool:"session"}` no op-log (mesmo shape das demais — um leitor, um formato) para
  tornar sessões contáveis.
- **Borda:** linha que não parseia (escrita parcial/corrupção) conta em
  `skipped_lines` e é pulada — cauda rasgada nunca falha o relatório. Supersedidas
  são história (S19): fora do peso morto, preview disponível no top.
- **Verificação:** unit tests do agregador (report.rs) + E2E CLI (`report --json`
  contra op-log real: top, peso morto, degradação sem log) + E2E MCP/CLI da linha
  "session" no initialize.

---

## P1 — FT: otimização do full-text (pré-launch — decisão do founder 2026-07-11)

> Origem: a task BQ1 (`ef_search` escalonado, [ADR 0015](adr/0015-ef-search-default-escalado-pelo-indice.md))
> isolou que o NFR `recall p99 @ 100k < 50 ms` (medido 1.224,62 ms — 24x acima) NÃO é
> causado pela busca vetorial (`Store::recall_vector` mede 19,32 ms no mesmo run, dentro
> do orçamento) — é o meio full-text da fusão híbrida que domina o tempo. Ver
> [ADR 0017](adr/0017-otimizacao-do-full-text-escopo-e-metodo.md) para o escopo completo
> e o porquê de profiling vir antes de qualquer otimização estrutural.

### S24. Profiling do meio full-text do `recall` @ 100k [⬜ pendente]

Como mantenedor, quero saber ONDE dentro do full-text o tempo é gasto antes de
escolher a otimização — sem isso, corrigir a estrutura errada é possível.

- **Dado** o dataset `agent-mem-100k` materializado, **quando** rodo o profiling
  dedicado desta story, **então** o resultado aponta, com evidência (não suposição), a
  fração do tempo de `Store::search_text`/`fts::search` gasta em: decodificação de
  postings (bytes → `Vec<Entry>`), I/O de página (cache miss / leitura de disco),
  hashing do `HashMap<Ulid, f32>` de scores, e recarga de registro pela closure
  `keep`/`doc_len` (re-tokenização).
- **Método:** ferramenta de profiling nativa da plataforma de desenvolvimento
  (flamegraph via `perf`/`samply`/equivalente Windows, ou instrumentação manual com
  `Instant` ao redor de cada fase de `fts::search` se a ferramenta de profiling não
  estiver disponível no ambiente) rodando uma sessão de queries repetidas sobre
  `agent-mem-100k` já aquecido (mesma metodologia do harness — `BENCHMARKS.md §3`).
- **Saída obrigatória:** relatório em `docs/adr/0017-otimizacao-do-full-text-escopo-e-metodo.md`
  (seção nova, "Resultado do profiling") ou arquivo próprio referenciado por ele —
  números concretos por fase, não texto qualitativo ("parece que é a decodificação").
  As tasks seguintes desta fase leem esse resultado antes de escolher o que
  implementar.
- **Borda:** se o profiling não conseguir isolar uma causa dominante clara (custo
  distribuído sem um pico óbvio), registrar isso também — é um resultado válido que
  muda a estratégia (early termination generic ataca todas as fases ao mesmo tempo).
- **Verificação:** o relatório existe, cita números, e a fração dominante soma > 60%
  do tempo total do meio full-text (ou o achado de "sem causa dominante" está
  explicitamente registrado).

### S25. Early termination no scan de postings [⬜ pendente — condicionada à S24]

Como agente que busca em corpus grande, quero que o `recall` não pague o custo de
decodificar/pontuar toda a postings list de um termo comum quando só os top-k
resultados importam.

- **Pré-requisito:** S24 concluída e aponta a decodificação/scoring do scan como
  fração relevante do tempo (se o profiling apontar outra causa dominante, esta story
  é reordenada ou substituída — ver ADR 0017 §3).
- **Dado** uma query cujo termo tem postings list grande, **quando** `fts::search`
  processa esse termo, **então** o scan corta cedo quando já há candidatos
  suficientes para o top-k final com confiança (critério exato — limiar de score,
  contagem de candidatos, ou heurística equivalente — decidido por medição e
  registrado em ADR, não escolhido a priori).
- **Dado** um corpus pequeno (early termination nunca dispara), **então** o resultado
  é IDÊNTICO ao scan completo — esta otimização não pode mudar quais documentos são
  retornados nem sua ordem em nenhum regime, só reduzir trabalho.
- **Formato:** sem mudança de `format_version` — é só o algoritmo de scan sobre a
  estrutura de postings já existente.
- **Verificação:** teste de equivalência (resultado com/sem early termination é
  idêntico byte a byte, corpus pequeno e grande) + `benches/run_all.sh --full` medindo
  o ganho de `query_engine_p50/p99_ms` @ 100k antes/depois.

### S26. Compressão delta+varint e/ou skip lists nas postings [⬜ pendente — condicionada à S24/S25]

Como mantenedor, quero reduzir o custo de I/O e decodificação por página quando o
scan em si (S25) não for suficiente para cumprir o NFR.

- **Pré-requisito:** S24 aponta I/O de página ou volume de bytes decodificados como
  fração relevante, OU S25 sozinha não fecha o NFR `recall p99 @ 100k < 50 ms`.
- **Dado** uma postings list persistida, **quando** o arquivo é escrito por este
  build, **então** os `record_id` (u128/ULID) são codificados como delta entre
  entradas consecutivas (a lista já é ordenada por id — S9/FORMAT.md §11) + varint,
  reduzindo bytes por entrada sem perder informação.
- **Dado** uma postings list grande o bastante para valer o overhead, **então** uma
  estrutura de skip (blocos de tamanho fixo com ponteiro/offset) permite pular blocos
  inteiros sem decodificá-los quando o scan (S25) já sabe que pode descartá-los.
- **Formato:** muda a codificação de `FTS_POSTINGS` — `format_version` sobe (bump
  aditivo, ADR 0017 §2). Um arquivo de versão anterior continua legível: decodificado
  pelo layout antigo, sem skip list nem compressão (degrada em desempenho, nunca em
  corretude ou em erro).
- **Verificação:** round-trip de leitura/escrita entre builds de `format_version`
  diferentes (arquivo antigo abre normalmente); crash test cobrindo as páginas
  `FTS_POSTINGS` no novo layout; `benches/run_all.sh --full` confirmando o NFR
  `recall p99 @ 100k < 50 ms` OU, se ainda não fechar, o ADR 0017 é atualizado com os
  números e a decisão do founder sobre prosseguir vs. documentar limitação.

### S27. Recall de pior-caso a 100k fechando o NFR de S16 [✅ implementada]

Como agente que faz uma busca "azarada" (query cujo vizinho semântico verdadeiro
está mal posicionado no grafo HNSW), quero que mesmo a pior query do lote ainda
recall o suficiente — não só a média.

- **Origem:** BQ1/S16 (task #38) reprovou o NFR de recall de pior-caso mesmo no
  degrau máximo medido (`ef_search = 256`): pior query = 0,20 contra o alvo ≥ 0,70.
  O sweep já mostrou a curva de recall médio achatando na faixa 192–384 — subir o
  `ef_search` de busca sozinho não deve resolver a cauda (ADR 0015, "Duas
  reprovações do DoD original", item 1).
- **Dado** o dataset `agent-mem-100k` e o conjunto de 1000 queries do harness,
  **quando** rodo `benches/run_all.sh --full` com a mudança desta story, **então**
  a pior query individual do lote atinge recall@10 ≥ 0,70 (ADR 0015/§NFR), sem
  regredir a média (continua ≥ 0,95) nem o p10/p50 já medidos.
- **Método (a decidir por medição, registrado em ADR novo):** candidatos a
  investigar, não escolhidos a priori — (a) revisitar `ef_construction`/`M` na
  CONSTRUÇÃO do índice (o ADR 0015 só tocou o lado da busca); (b) um degrau de
  `ef_search` maior que 256, medindo o custo de latência adicional contra o
  orçamento restante do NFR de p99; (c) heurística de retry/expansão quando o
  vizinho mais próximo aparenta baixa confiança.
- **Borda:** a solução não pode regredir latência/RSS além dos limiares do
  BENCHMARKS.md §5 nem contradizer os degraus já medidos e aceitos (ADR 0015) sem
  atualizar o ADR com a nova medição.
- **Verificação:** `benches/run_all.sh --full` nos dois datasets + `cargo test
  --workspace`; ADR novo registrando o método escolhido e os números antes/depois.
- **Resultado (2026-07-12, ADR 0019):** o probe de diagnóstico
  (`benches/src/bin/probe_worst.rs`, 1000 queries, dupla notação) mostrou que a
  cauda era 100% **artefato de empate da métrica**, não miss do HNSW: o corpus
  tem 23,0% de textos duplicados exatos @ 100k (embeddings bit-idênticos), a
  fronteira do top-10 exato é um platô de 14–29 scores empatados nas queries
  "ruins", e todas as 70 queries abaixo de 0,70 têm paridade de score 1,00.
  Nenhum dos candidatos (a)/(b)/(c) foi adotado — descartados pelo dado. O
  grading do harness virou tie-aware (paridade de score, estilo ann-benchmarks,
  mesma régua para os concorrentes): @ 100k média 0,9360 → 1,0000 e pior query
  0,20 → 1,00; @ 10k 0,9953 → 1,0000 e 0,90 → 1,00. Engine, formato e
  parâmetros HNSW intocados. Rerun oficial de `benches/run_all.sh --full` fica
  com o founder (execução longa, fora da sessão).

Como mantenedor, quero que o pico de memória a 100k memórias volte a caber no NFR
de 300 MiB — hoje passa por pouco, mas passa.

- **Origem:** BQ1/S16 mediu RSS de pico 307,1 MiB (query) / 305,4 MiB (ingest) a
  100k, contra o teto de 300 MiB — a folga de 6% que a task citava como
  disponível (280,9 MiB medidos antes) já não existe nessa escala. O ADR 0015
  registra que a causa é "dimensionamento geral do índice a 100k", não um efeito
  colateral do `ef_search` escalado (que consome mais RAM durante a BUSCA, mas a
  folga já estava apertada antes dessa mudança) — **precisa de investigação
  própria**, ainda não feita.
- **Dado** o dataset `agent-mem-100k` materializado, **quando** rodo o profiling
  de memória desta story (heap profiler nativo da plataforma, ou instrumentação
  manual de alocação por fase — mesmo espírito de método do profiling da S24),
  **então** o resultado aponta que estrutura domina o pico: grafo HNSW em memória,
  cache de páginas do pager, buffers de decodificação de postings/registros, ou
  outra.
- **Dado** a causa identificada, **então** a correção (redução de over-allocation,
  ajuste de tamanho de cache, streaming em vez de buffer completo — a decidir pelo
  profiling, não a priori) traz o pico de volta para < 300 MiB @ 100k sem regredir
  recall ou latência além dos limiares do §5.
- **Verificação:** `benches/run_all.sh --full` confirmando RSS de pico (ingest e
  query) < 300 MiB @ 100k; `cargo test --workspace`; ADR novo se a correção mudar
  uma decisão de dimensionamento já registrada em ADR anterior (ex.: ADR 0002/0008
  do HNSW).

---

## Requisitos não-funcionais consolidados

| Requisito | Alvo | Verificação |
|---|---|---|
| Durabilidade | zero perda confirmada sob kill -9/queda de energia | crash harness (S4) |
| Integridade | arquivo nunca irrecuperável; recovery automático | crash harness + fuzzing |
| Latência `recall` | < 50 ms p99 @ 100k memórias, CPU-only | benchmark harness |
| Recall@10 @ 100k | ≥ 0,95 média · ≥ 0,70 pior query (alvos propostos — S16) | benchmark harness |
| Latência `remember` | < 200 ms p99 (dominada pelo embedding) | benchmark harness |
| Artefato de release | < 40 MB comprimido, incluindo modelo (ADR 0010) | CI de release |
| RAM | < 300 MB @ 100k memórias | benchmark harness (RSS) |
| Plataformas | Windows, Linux, macOS — mesmo arquivo, little-endian | CI matriz 3 SOs |
| Rede | ZERO chamadas no núcleo (auditável) | revisão + ausência de deps de rede |
| Robustez de código | `unsafe_code = forbid`; `unwrap/expect/panic = deny` na engine | lints de workspace no CI |
