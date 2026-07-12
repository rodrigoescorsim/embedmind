# ADR 0020 — RSS de pico @ 100k era o harness, não o engine

**Status:** Aceito (jul/2026). Story S28 / task FT5 ([03-tasks.md](../03-tasks.md)) —
corrige o estouro de RSS registrado no [ADR 0015](0015-ef-search-default-escalado-pelo-indice.md).

## Contexto

O ADR 0015 mediu RSS de pico @ 100k em 307,1 MiB (query) / 305,4 MiB (ingest),
acima do teto de 300 MiB (`docs/BENCHMARKS.md` §5), e registrou a causa como
"dimensionamento geral do índice a 100k, nunca investigado a fundo" — uma
suposição, não uma medição. A story S28 pede profiling de memória antes de
qualquer correção (mesmo método da FT1/ADR 0017: medir, não ler código e
adivinhar).

## Profiling

Binário novo `benches/src/bin/profile_rss.rs` (`cargo run -p embedmind-bench
--release --bin profile_rss -- agent-mem-100k`): amostra RSS do processo a
cada fronteira de fase do harness (`benches/src/sysmem::RssSampler`, já
existente desde a S24) e mede o pico dentro de cada fase medida.

Resultado real, `agent-mem-100k` (log completo em
`benches/results/profile-rss-100k.txt`):

| fase | RSS (MiB) |
|---|---|
| process start | 11,8 |
| após carregar o embedder ONNX | 88,6 |
| após carregar o `VectorSet` (.vec, 100k vetores) | 241,3 |
| após `Store::open` | 241,3 |
| pico da fase de recall (50 queries) | 254,6 |
| pico da fase de warm-query (40 queries) | 260,8 |
| **após dropar o `VectorSet`** | **93,0** |
| pico da fase de warm-query, `VectorSet` já dropado (40 queries) | 97,8 |
| pico da fase de ingest (200 remembers) | 94,9 |

Achados:

- **`Store::open` não move o ponteiro** (241,3 → 241,3 MiB): confirma o que o
  ADR 0008 já previa — o HNSW é paginado com endereçamento direto, sem tabela
  de localização residente, e o pager não mantém cache de páginas. O engine em
  si não é a estrutura dominante.
- **O `VectorSet` brute-force do harness** (`.vec` sidecar carregado inteiro
  em memória para servir de baseline "verdade" ao `recall@10`) é quem domina:
  +152,7 MiB no load (88,6 → 241,3), e ao dropá-lo o RSS cai para 93,0 MiB —
  o alocador devolve a memória ao SO de fato, não é fragmentação.
  `VectorSet` é aparelhagem do harness (`docs/BENCHMARKS.md` §3), não memória
  de produto: um servidor MCP real nunca carrega essa estrutura.
- O `run_suite` original (`benches/src/harness.rs`) segurava o `VectorSet` por
  referência durante **todas** as fases, incluindo as duas medidas por
  `peak_rss_query_mib`/`peak_rss_ingest_mib` — mesmo depois de seu único
  consumidor (a fase de recall) já ter terminado. Por isso os 307,1/305,4 MiB
  do ADR 0015 misturavam RSS de produto com RSS de aparelhagem de medição.
- O custo de produto real (ONNX + engine + fusão híbrida) fica em ~98 MiB no
  pico de query e ~95 MiB no pico de ingest @ 100k — bem abaixo do teto, com
  folga de ~67%.

## Confirmação oficial (`benches/run_all.sh --full`)

A rodada completa do harness (1000 queries, os dois datasets, log completo em
`benches/results/run-all-full-s28.log`, JSON em
`benches/results/0.1.0-dev.json`) confirma a correção fora do binário de
diagnóstico isolado:

| NFR | Alvo | Medido @ 100k | Veredito |
|---|---|---:|:---:|
| peak RAM @ 100k | < 300 MiB | **120,6 MiB (query) / 120,1 MiB (ingest)** | ✅ pass |

Os números da rodada oficial (peak RSS via o processo do próprio harness, não
via `profile_rss` isolado) ficam um pouco acima dos ~98 MiB medidos no binário
de diagnóstico — esperado, já que a rodada oficial mede 1000 queries reais
(vs. 40 do `profile_rss`) e outras fases do harness ao redor — mas seguem bem
abaixo do teto de 300 MiB, com folga de ~60%.

`recall p99 @ 100k` reprova nesta mesma rodada (956,80 ms vs. alvo < 50 ms) —
gargalo pré-existente do full-text (ADR 0017/0018), fora do escopo desta
story (FT5 é independente de FT1-FT3, `docs/03-tasks.md`). Não é regressão
introduzida por esta correção: o `query engine` já carregava esse custo antes.

## Decisão

**`harness::run_suite` passa a receber o `VectorSet` por valor e o dropa logo
após a fase de recall** (seu último consumidor), antes das fases de warm-query
e de ingest que os campos `peak_rss_query_mib`/`peak_rss_ingest_mib` medem.
`benches/src/bin/run_all.rs` recarrega o `.vec` sidecar depois, só para a
comparação com concorrentes (`competitors::run_all`), que roda fora das fases
medidas — recarregar é barato perto de deixar a estrutura contaminar o NFR.

Nenhuma mudança no engine (`embedmind-core`): a causa nunca foi o
dimensionamento do HNSW ou do pager. **O ADR 0015 fica corrigido** — a frase
"causa é dimensionamento geral do índice a 100k" nessa entrada estava errada;
a nota lá é mantida (ADRs são imutáveis após aceitos) com uma remissão a este
ADR.

## Alternativas rejeitadas

- **Reduzir o `ef_search` no maior degrau (ADR 0015) para economizar RAM
  durante a busca:** endereçaria um sintoma que não é a causa — o beam do
  HNSW nunca apareceu como estrutura residente relevante no profiling — e
  reabriria a reprovação de recall de pior-caso que o ADR 0015 já documenta
  como dívida técnica.
- **Adicionar cache/streaming no pager:** o profiling mostra que o pager já
  não retém páginas (`Store::open` não move RSS); não há estrutura de produto
  para otimizar aqui.
- **Medir RSS só no processo do servidor real, sem tocar o harness:** não
  reproduz o número — o gap do NFR só aparece porque o harness mede RSS do seu
  próprio processo, que inclui a aparelhagem de baseline. Corrigir a
  aparelhagem é a correção certa, não mudar o que o NFR mede.

## Consequências

- `harness::run_suite(spec, data_dir, store, set: VectorSet, embedder, opts)`
  agora consome `set`; único chamador é `run_all.rs`, atualizado.
- RSS de pico @ 100k medido: ~97,8 MiB (query) / ~94,9 MiB (ingest) no
  diagnóstico isolado (`profile_rss`); **120,6 MiB (query) / 120,1 MiB
  (ingest) na rodada oficial** `benches/run_all.sh --full` (1000 queries) —
  ambos dentro do teto de 300 MiB com folga larga. Story S28 fechada.
- `benches/src/bin/profile_rss.rs` fica no repo como ferramenta de diagnóstico
  reutilizável, mesmo padrão do `profile_fts.rs` (S24/ADR 0017) — próxima
  suspeita de RSS não precisa reinventar a instrumentação.
- Nenhum ADR de dimensionamento do HNSW (0002/0008) precisou ser revisitado: a
  causa nunca foi lá.
