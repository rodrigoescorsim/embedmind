# ADR 0026 — Corpus com localidade de sessão + Zipf não reabilita o BlockMax-WAND

**Status:** Aceito (2026-07-13). Executa **BMW-5**, a revisão pós-BMW pedida pelo
founder: investigar se o *benchmark* subestimava o BlockMax-WAND (BMW) por não
refletir a distribuição real de memórias de agente. Conclusão medida: **não
reabilita** — a limitação de eficácia do BMW documentada na
[BMW-3](0025-blockmax-wand-na-busca-fts.md) é do algoritmo/formato sobre este
padrão de dado, **não** um artefato da metodologia de benchmark. Atualiza a
leitura honesta do [ADR 0017](0017-otimizacao-do-full-text-escopo-e-metodo.md).

Não mexe no algoritmo do BMW nem no formato do arquivo — só no **gerador de
corpus** (`benches/src/corpus.rs`) e nesta documentação.

## Contexto — a dúvida deixada em aberto pela BMW-3

A fase BMW fechou com o NFR de latência reprovado (`recall` p99 @100k = 224,00 ms,
teto 50 ms) e uma causa raiz medida ([ADR 0025](0025-blockmax-wand-na-busca-fts.md)
§BMW-3): o BMW ativa em 82,8% das queries oficiais, mas só pula **0,05%** dos
~2,87 M blocos tocados sem decodificar. O refinamento block-max só evita
decodificar um bloco quando o "pouso" da busca cai exatamente no `first_id` do
bloco seguinte (`BmwCursor::advance_to`, `fts.rs`); senão decodifica o bloco de
destino mesmo assim.

A BMW-3 **levantou, sem confirmar**, uma suspeita: o corpus sintético
(`corpus::generate`) sorteia templates/slots **uniformemente** por um splitmix64,
sem localidade temporal nem lei de Zipf. Memória real de agente é diferente —
sessões de trabalho geram rajadas de memórias sobre o mesmo projeto/termo em
janelas de tempo contíguas (ULIDs próximos), e o vocabulário segue Zipf (poucos
termos dominam, cauda longa). A hipótese: essa contiguidade daria ao bound
block-max a chance de provar um bloco inteiro abaixo do limiar, que a dispersão
uniforme nunca dá.

**BMW-5 mede essa hipótese, sem assumi-la.**

## O que foi feito

- **Gerador de corpus ganhou um modo `Locality`** (`corpus::generate_local`),
  ao lado do `Uniform` existente (`corpus::generate`) — que **não** foi tocado e
  segue sendo o corpus oficial de regressão (o "pior caso documentado"). O modo
  novo:
  - **Localidade de sessão:** memórias emitidas em *rajadas* — uma sessão fixa um
    projeto e um "assunto" (`Slots`) e escreve `SESSION_LEN` (400 ± 200) memórias
    consecutivas, cada uma variação leve do assunto. Como o store atribui ULIDs
    na ordem de ingest, uma rajada cai numa **janela de ids contígua**: as
    postings de um termo quente se agrupam em vez de se espalharem por todo o
    espaço de ids.
  - **Vocabulário Zipf:** os slots são sorteados com peso ∝ 1/(rank+1)
    (`Rng::zipf`/`pick_zipf`), então poucos termos de cabeça dominam (cruzando
    o `SKIP_MIN_DOC_FREQ` = 512 que transforma as postings num skip index real)
    e uma cauda longa fica rara.
  - Determinístico por seed, com testes (determinismo, contagem exata, rajadas
    contíguas vs. uniforme, cabeça Zipf domina a cauda).
- **Dataset novo `agent-mem-locality-10k`** (mesmo seed e tamanho do
  `agent-mem-10k`, modo `Locality`), materializado em fv6. `query_texts` segue a
  distribuição do dataset que sonda (localidade é consultada com textos de
  localidade, batendo no vocabulário de cabeça — exatamente onde o BMW atuaria).
- **Suite completa rodada lado a lado** @10k (`bmw_reach` para blocos pulados;
  `run_all` para recall/latência/lift) sobre os dois corpora.

## Medição — os dois corpora lado a lado, @10k, k=10, 1000 queries

Ambiente: Windows, CPU-only, model `all-MiniLM-L6-v2-int8`, arquivos fv6 (BMW
ativo).

### Alcance do BMW (`bmw_reach`)

| métrica | `agent-mem-10k` (uniforme) | `agent-mem-locality-10k` |
|---|---:|---:|
| queries com ≥1 termo que pulou um bloco | 459 (45,9%) | **18 (1,8%)** |
| queries onde todo termo decodificou inteiro | 541 (54,1%) | 982 (98,2%) |
| `blocks_total` | 296 635 | 197 144 |
| `blocks_skipped` | 901 (0,3%) | **18 (0,0%)** |
| `pivot_skips` | 131 256 | 50 430 |
| `docs_evaluated` | 951 859 | 869 580 |

### Recall / latência / lift (`run_all`)

| métrica | uniforme | localidade |
|---|---:|---:|
| recall@10 (HNSW puro, tie-aware) | 1,0000 | 0,9861 |
| `query engine` p99 (sem embed) | 133,21 ms | **83,86 ms** |
| `query` p99 (com embed, end-to-end) | 170,65 ms | 121,15 ms |
| lexical lift: híbrido vs vetor-puro | +0,09 (1,00 vs 0,91) | +0,21 (1,00 vs 0,79) |

## Leitura — a suspeita foi **refutada**

A localidade + Zipf faz o BMW pular **menos** blocos, não mais: 0,0% (18/197 144)
contra 0,3% (901/296 635) do corpus uniforme, e a fração de queries com alcance
real do BMW despenca de 45,9% para 1,8%. **O oposto da hipótese.**

Por quê, mecanicamente: o refinamento block-max precisa de **heterogeneidade de
impacto entre blocos** — um bloco só é pulável quando seu `max_term_freq`
(o bound de impacto, [ADR 0024](0024-bound-de-impacto-por-bloco-fv6.md)) prova
que ele está abaixo do k-ésimo score corrente θ. A localidade agrupa ids
contíguos, mas ao concentrar as ocorrências de um termo quente ela deixa o
impacto **alto e uniforme em praticamente todos os blocos** daquele termo — o
bound raramente cai abaixo de θ, então quase nenhum bloco é pulável. A
contiguidade de ids ajuda o *cursor* a avançar (o WAND salta ranges de id), mas
não é o que o *refinamento block-max* precisa; o que ele precisa é justamente
blocos com impacto baixo intercalados, que nem a distribuição uniforme nem a de
localidade oferecem neste vocabulário sintético pequeno.

O `query engine` p99 **é** menor no corpus de localidade (83,86 vs 133,21 ms),
mas **não** porque o BMW pula mais blocos — pula menos. É porque a Zipf
concentra o vocabulário em menos termos e as queries de localidade batem em
postings mais rasas no agregado (`blocks_total` 197 k vs 296 k): há simplesmente
menos trabalho a fazer, independentemente do BMW. Atribuir essa queda ao BMW
seria desonesto; a instrumentação `bmw_reach` mostra que o BMW contribui com
0,0% de blocos pulados.

**Decisão @10k basta — não pedimos a rodada @100k.** A direção do efeito é
inequívoca e mecanicamente explicada (menos blocos pulados, não mais); rodar
@100k (execução longa) só confirmaria a mesma direção com números maiores. Se
uma revisão futura quiser o número @100k para o registro, o comando é
`gen_dataset agent-mem-locality-100k` (adicionar o spec) + `bmw_reach`, mas ele
não muda a conclusão.

## Consequências

- **A limitação do BMW é do algoritmo/formato, não da metodologia.** A dúvida
  aberta na BMW-3 fica fechada: o corpus uniforme não era pessimista *demais*;
  a distribuição realista de memória de agente (localidade + Zipf) é, se algo,
  **pior** para o refinamento block-max, não melhor. O ADR 0017 é atualizado
  com essa conclusão.
- **Nenhuma promessa sobre o NFR.** Não temos dado de produção real, e o dado
  sintético que temos não sustenta que o NFR <50 ms feche — reforça o contrário.
  Nada aqui reabre o critério de reversão do
  [ADR 0023](0023-blockmax-wand-decisao-fase-bmw.md) (aceitar a limitação
  documentada vs. voltar ao vector-only default): isso segue sendo decisão do
  founder, **não tomada aqui**.
- **O corpus de localidade fica no arsenal de benchmark** como modo adicional
  (não no caminho de regressão oficial), útil para futuras investigações de
  padrão de dado. O corpus uniforme continua sendo o baseline versionado.
- **Sem mudança de código de produção nem de formato.** Só gerador de corpus e
  documentação.
