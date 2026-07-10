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

### S16. Recall estável em escala — `ef_search` proporcional ao índice [⬜ pendente]

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

### S18. Comparação com concorrente da categoria de produto [⬜ pendente]

sqlite-vec/zvec são baselines de camada de índice; a alternativa que um dev de agente
realmente considera é um vector store local que também embeda (Chroma em modo
local/embedded, com o mesmo all-MiniLM-L6-v2, é a briga justa).

- **Dado** o harness com a feature de comparação habilitada, **quando** rodo a suite,
  **então** o Chroma (modo local, versão pinada) aparece na seção texto→resultado com
  recall@10, p50/p99 e tamanho em disco, sob as mesmas regras de honestidade (S15);
  toolchain ausente reporta "not measured", nunca número inventado.
- **Verificação:** `benches/run_all.sh` com a feature ligada num ambiente com Python
  disponível (dependência externa do founder, como as toolchains de sqlite-vec/zvec).

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

### S20. Recência na fusão do recall [⬜ pendente]

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

### S21. Curadoria na escrita — near-duplicates no `remember` [⬜ pendente]

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

### S22. Op-log estruturado do servidor [⬜ pendente]

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
