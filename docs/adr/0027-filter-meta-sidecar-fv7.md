# ADR 0027 — Filter-meta sidecar (`format_version` 7)

**Status:** Aceito (jul/2026). Executa **FTOPT-1**, redesenhada sobre o dado da
FTOPT-0. Outro bump **aditivo** de `format_version` (mesmo padrão dos ADRs
0021/0022/0024): arquivo de versão anterior continua legível e gravável no seu
layout; `vacuum` é o caminho de upgrade.

## Contexto

O profiling FT1 ([ADR 0017](0017-otimizacao-do-full-text-escopo-e-metodo.md))
mediu a closure `keep` — recarga do registro completo (`btree::get` +
`MemoryRecord::decode`) por candidato avaliado, só para reler
`tombstone`/`superseded`/`project`/`agent` — em **88,8% do tempo** do meio
full-text do recall híbrido @ 100k (p50 994 ms). O corpo original da FTOPT-1
propunha a estrutura clássica dos motores maduros (Lucene DocValues, tantivy
fast fields): metadados de filtro numa estrutura columnar leve, fora do corpo
do documento, para **pular o I/O nos candidatos rejeitados**.

A FTOPT-0 (profiling confirmatório, `KeepOutcome`, ADR 0017) invalidou essa
premissa: a fração de candidatos **rejeitados** *cai* com a escala — 2,1% @10k,
**0,1% @100k**. Uma estrutura que só evita recarregar rejeitados tem teto de
ganho de ~0,1% no corpus onde o NFR reprova.

## A decisão de desenho (rota 1 da FTOPT-0)

O dado mata o desenho original, não a estrutura. A observação central:

> O custo do `keep` **não é intrínseco aos candidatos aceitos**. `keep` não
> precisa do conteúdo — precisa da **decisão** (vivo? no escopo? do agente?).
> E `doc_len` (normalização BM25) não precisa do conteúdo — precisa da
> contagem de tokens, que é **imutável** depois do `remember` (conteúdo nunca
> é editado; `forget`/supersede só mudam flags).

O registro completo só é necessário em dois pontos:

1. **Montar os top-k `Hit`s devolvidos** — `k` (≈ 8–10) registros por query,
   não um por candidato avaliado (milhares @100k).
2. **Filtros de metadados customizados** (`record_passes_filters`) — só em
   queries que os usam, e mesmo aí um bit `has_metadata` no sidecar rejeita
   sem carga os registros que não têm metadado nenhum (filtro sobre chave
   ausente é non-match plano, nunca erro de tipo).

Logo o sidecar `record_id → (flags, project_sym, agent_sym, doc_len)` serve
candidatos **aceitos e rejeitados** igualmente: elimina a recarga do B-tree
para todo candidato avaliado, atacando os ~93% do tempo medido (keep 88,8% +
doc_len 4,5%), não os 0,1%. Continua sendo exatamente "metadados leves fora do
registro completo" — o escopo do corpo da task — sem virar cache de conteúdo.

**Guard-rail:** esta task decide o desenho técnico com base no dado; ela não
decide estratégia de produto. A medição do ganho real @100k fica para a
FTOPT-4; se o número não fechar o NFR, a decisão sobre o que fazer (FTOPT-2,
aceitar o NFR, outra rota) continua sendo do founder — nada aqui a antecipa.

## Layout (`docs/FORMAT.md` §13)

Duas cadeias de páginas, ambas **newest-first** (`next_page` aponta para a
página mais velha; um append reescreve no máximo a página head):

- **`FILTER_META` (0x0C)** — entries fixas de **29 bytes**:
  `record_id` (16, big-endian ULID) + `flags` (1: bit0 tombstone, bit1
  superseded, bit2 has_metadata, bit3 scope_overflow) + `project_sym` (u32) +
  `agent_sym` (u32) + `doc_len` (u32). Updates (forget/supersede) **appendam**
  uma entry nova para o mesmo id; a mais nova vence na materialização;
  `vacuum` reescreve denso.
- **`FILTER_SYMBOLS` (0x0D)** — tabela de símbolos `u32 → string` dos
  projects/agents internados. **Exata, sem hash**: um hash curto colidindo
  entre dois projetos devolveria resultado errado — equivalência vence
  compacidade. Símbolo 0 = "sem string" (project global / agent vazio); uma
  string que não cabe numa página (ou u32 esgotado) marca a entry com
  `scope_overflow` e queries com escopo caem no registro — corretude sobre
  velocidade, nunca erro.

Header: `filter_meta_page` (offset 172) e `filter_symbols_page` (offset 180),
bytes reservados-zero até o fv6 — arquivo antigo decodifica com roots 0 = sem
sidecar, e o `keep` degrada para a recarga completa (o comportamento
pré-FTOPT-1), nunca erro.

Tamanho: 29 B × 100k ≈ 2,9 MiB materializados em memória — negligível frente
ao teto de 300 MiB de RSS já validado (ADR 0020).

## Escrita: mesma transação, sempre

Todo caminho que grava um `MemoryRecord` (`remember`, `forget`, supersede,
`insert_record` do vacuum) deriva a entry do sidecar **dos mesmos bytes** que
está gravando e a appenda **na mesma `Txn`** — o sidecar e o registro se
tornam duráveis atomicamente ou nenhum dos dois (mesma garantia WAL do resto
do formato). Em arquivo fv ≤ 6 a escrita é no-op (o arquivo não muda de
layout; regra dos ADRs 0021/0022/0024).

## Leitura: decidir sem carregar; na dúvida, carregar

`Store` materializa o sidecar inteiro num `HashMap` em memória, cacheado por
`txn_counter` (qualquer commit invalida naturalmente; `vacuum` invalida
explicitamente porque o arquivo novo reinicia o contador). Por query, as
strings de escopo/agent resolvem para símbolos **uma vez** (`want_project`/
`want_agent` — com as bordas de string vazia espelhando `in_scope`:
`Scope::Project("")` nunca casa; `agent("")` casa agent vazio). Por candidato,
`Table::decide` responde:

- **Accept/Reject** — sem tocar no B-tree (o caso de ~100% dos candidatos em
  queries sem filtro de metadado);
- **NeedRecord** — entry ausente, `scope_overflow`, ou filtros custom sobre
  registro que tem metadados: cai no predicado completo sobre o registro,
  byte-idêntico ao caminho antigo. **Nunca errado, às vezes indeciso.**

`doc_len` vem da entry (capturado na escrita do mesmo conteúdo que
`index_document` tokenizou; conteúdo é imutável ⇒ nunca fica stale).

`near_duplicates` (caminho de escrita do `remember_detailed`, poucos
candidatos) segue no caminho antigo deliberadamente — sem ganho que pague o
risco.

## Alternativas rejeitadas

- **Só evitar recarga nos rejeitados** (o desenho original): teto de 0,1%
  @100k pela FTOPT-0 — descartado pelo dado.
- **Hash curto do project em vez de tabela de símbolos**: colisão devolve
  resultado errado; a suite de equivalência é o critério de pronto, então
  qualquer probabilidade de erro > 0 por construção é inaceitável.
- **Cache de conteúdo completo (LRU de registros)**: reabriria discussão de
  arquitetura (memória por conteúdo, política de despejo) fora do escopo de
  "metadados leves"; fica como alternativa registrada na FTOPT-0 caso o
  ganho desta estrutura não feche o NFR.
- **B-tree secundário em vez de cadeia append-only**: reintroduziria
  exatamente o traversal por candidato que está sendo eliminado; a cadeia
  materializada em RAM é O(1) por candidato e o custo de materialização é
  pago uma vez por estado commitado.

## Testes (critério de pronto da FTOPT-1)

- **Equivalência** (`tests/filter_meta.rs`): oráculo = arquivo fv6 genuíno
  (`PagerOptions::format_version`, mesma receita dos testes cross-version do
  FTS) com o mesmo workload; `search_text` idêntico em toda a matriz de
  queries (escopo hit/miss/vazio, agent hit/miss/vazio, filtros custom,
  chave ausente, combinações), incluindo pós-`forget`, pós-supersede,
  pós-reopen e pós-`vacuum` (upgrade fv6 → fv7 verificado nos bytes do
  header).
- **Crash** (`tests/crash_filter_meta.rs`): sweep de injeção (Before/Torn por
  operação de I/O) sobre workload com todos os caminhos de escrita; após cada
  recovery, `verify_filter_meta_invariant` prova que o sidecar concorda com
  os registros (entry presente, decisão de liveness, símbolos do próprio
  escopo, `doc_len`).
- **Fuzz** (`fuzz_filter_meta_page`, `docs/TESTING.md` §3): ambos os decoders
  sobre bytes arbitrários — toda aritmética checked, counts validados contra
  o tamanho da página antes de alocar, ciclo de cadeia é erro tipado (lição
  do crash `fts.rs:325`/`lookup_via_skip`); seeds reais no corpus.

## Consequências

- Formato ganha 2 page types e 2 campos de header; nenhum byte existente
  muda de significado (checklist §10 do FORMAT.md, regra 1).
- Escrita paga ~29 bytes + (raro) interning por registro — O(1) amortizado,
  no mesmo commit.
- A primeira query após um commit paga a rematerialização (O(n) no total de
  entries; ~2,9 MiB @100k). Workload de rajada de queries amortiza para ~0;
  workload write-heavy intercalado pode fazer disso um custo visível — se a
  FTOPT-4 mostrar isso, a atualização incremental do cache é a evolução
  natural (o writer conhece as entries que appendou).
- O ganho real @100k **não está medido aqui** — é o objeto da FTOPT-4
  (mesma régua do ADR 0017). Este ADR registra desenho e equivalência, não
  reivindica número.
