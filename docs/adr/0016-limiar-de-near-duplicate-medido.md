# ADR 0016 — Limiar de near-duplicate do `remember` medido no corpus (0.80)

**Status:** Aceito (jul/2026). Story S21 / task FR3 ([01-spec.md](../01-spec.md),
[03-tasks.md](../03-tasks.md)) — curadoria na escrita: o `remember` reporta
memórias existentes parecidas (`similar: [...]`) sem nunca bloquear a gravação.

## Contexto

A S21 exige um limiar de similaridade acima do qual uma memória existente é
reportada como near-duplicate — e a spec exige que o valor venha de **medição
no corpus do harness, nunca de palpite**. O custo de errar é assimétrico: a
lista informa, não bloqueia, então um falso positivo raro é um aviso espúrio
barato; já um limiar baixo demais transformaria toda gravação num ruído de
"parecidas" inúteis.

Medição: `benches/src/bin/calibrate_near_dup.rs`, seeds fixos (reproduzível
run a run), modelo embarcado `all-MiniLM-L6-v2-int8` (ADR 0004). Dois lados:

- **400 pares duplicados** (`corpus::duplicate_pairs`): 200 *paraphrase*
  (mesmos fatos — mesmos slot fills —, template diferente, podendo cruzar a
  fronteira pt-BR/en do corpus) e 200 *noisy copy* (o mesmo texto com moldura:
  prefixo "Update:", sufixo "— sem mudanças." etc. — o caso real de um agente
  re-gravando um fato).
- **2000 pares não-relacionados**: memórias distintas da distribuição padrão
  do corpus (`corpus::generate`).

Distribuições de cosseno medidas (run 2026-07-11, determinístico):

| distribuição | min | p5 | p50 | p95 | p99 | max |
|---|---|---|---|---|---|---|
| duplicados: noisy copy | 0.7585 | **0.8403** | 0.9249 | 0.9623 | 0.9681 | 0.9773 |
| duplicados: paraphrase | -0.0642 | 0.0108 | 0.5665 | 0.8113 | 0.8648 | 0.8954 |
| não-relacionados | -0.0872 | 0.0357 | 0.2450 | 0.5140 | **0.6391** | **0.8104** |

Sweep de candidatos (fração pega / fração de não-relacionados flagrada):

| limiar | noisy caught | paraphrase caught | unrelated flagged |
|---|---|---|---|
| 0.750 | 100.0% | 13.5% | 0.20% |
| 0.775 | 99.5% | 10.5% | 0.15% |
| **0.800** | **98.5%** | **7.0%** | **0.10%** |
| 0.825 | 96.5% | 4.0% | 0.00% |
| 0.850 | 94.5% | 2.0% | 0.00% |

## Decisão

**`NEAR_DUP_THRESHOLD = 0.80`** (cosseno, vetores normalizados), constante
pública em `embedmind-core::api`, com `NEAR_DUP_LIMIT = 5` e snippet de
`NEAR_DUP_SNIPPET_CHARS = 160` caracteres (corte nunca no meio de um char).

- 0.80 fica **acima** do p99 dos não-relacionados (0.639) com margem, e
  **abaixo** do p5 dos noisy copies (0.840): pega 98.5% do caso-alvo com
  0.10% de falso positivo. Como a lista informa e nunca bloqueia, o falso
  positivo residual (o max não-relacionado é 0.810 — colisões raras existem)
  é o lado barato do trade.
- A varredura reusa o embedding do próprio `remember` (zero embedding extra):
  o scan roda com os chunks já computados, **antes** da transação inserir os
  vetores — a memória nova nunca casa consigo mesma. Filtros idênticos aos de
  qualquer busca (ADR 0003/0013): só vivas, não-superseded e do MESMO escopo
  aplicado (projeto exato, ou global para draft global — nunca `Scope::All`).
- Score reportado é o cosseno cru, não rank RRF: isto é um teste de duplicata
  em escala única, não fusão de listas — o ADR 0005 governa o ranking do
  recall, não isto.

## Limitação registrada (honestidade)

No limiar escolhido, **paraphrases sintéticas quase não são pegas (7%)**: o
p50 delas é 0.566 porque o corpus mistura pt-BR/en e o template trocado
frequentemente cruza o idioma — e o MiniLM embarcado não é multilíngue
(limitação do modelo, ADR 0004, não do limiar; nenhum limiar separa essa
distribuição, que se sobrepõe quase inteira aos não-relacionados). O alvo da
S21 é o caso operacionalmente dominante — o agente re-grava o mesmo fato com
moldura — e esse é coberto a 98.5%. Se o modelo embarcado um dia virar
multilíngue, re-rodar `calibrate_near_dup` e revisitar a constante (o teste
`near_duplicates_respect_the_measured_threshold` tem um tripwire que força a
revisão dos pares se a constante sair de (0.75, 0.875]).

## Alternativas rejeitadas

- **Limiar por palpite (ex.: 0.9 "parece alto o bastante"):** violaria a
  exigência da spec e perderia 26.5 pts dos noisy copies (72% a 0.90) sem
  ganho — os não-relacionados já estão zerados bem antes.
- **Limiar adaptativo (percentil do arquivo, ou por projeto):** estado novo a
  manter e um comportamento não-reproduzível entre arquivos; a distribuição
  medida é estável o suficiente para uma constante auditável com a medição ao
  lado — mesmo espírito do ADR 0015 (degraus medidos, não fórmula).
- **Bloquear/deduplicar automaticamente acima de um segundo limiar:** decisão
  destrutiva sem contexto — quem tem contexto para julgar é o chamador
  (forget / supersedes / manter), exatamente o desenho da S21.
- **Re-embedar a query de near-dup separadamente:** custo de um embedding
  extra por `remember` para obter os mesmos vetores; o reuso é gratuito e é
  requisito explícito da story.

## Consequências

- `Store::remember_detailed` (novo) devolve `Remembered { memory, similar }`;
  `Store::remember` continua existindo com a assinatura antiga. MCP e CLI
  passam a usar `remember_detailed` — o campo `similar` é aditivo no MCP
  (clientes pré-S21 ignoram) e o CLI imprime
  `memória parecida existente: <id> — <trecho>`.
- O harness de ingest mede `remember_detailed` (o caminho real do MCP desde a
  S21), então o NFR `remember` p99 < 200 ms passa a incluir o scan de
  near-dup — o número publicado é o custo de verdade.
- Re-calibração: `cargo run -p embedmind-bench --release --bin
  calibrate_near_dup` (seeds fixos; `DUP_PAIRS`/`UNRELATED_PAIRS` para
  amostras maiores). Obrigatória ao trocar o modelo embarcado.
