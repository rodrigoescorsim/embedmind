# ADR 0023 — BlockMax-WAND para fechar o NFR de latência do full-text (decisão do founder)

**Status:** Aceito (2026-07-13). Decide o que ficou pendente no fechamento da fase FT
([ADR 0017](0017-otimizacao-do-full-text-escopo-e-metodo.md) §"Fechamento da fase FT" e
§"O benefício do full-text: queries lexicais") e abre a fase BMW (`ROADMAP.md`).

## Contexto

O ADR 0017 fechou a contabilidade de números da fase FT (2026-07-13) com o NFR de latência
ainda reprovado — `recall` p99 @ 100k em 224,88 ms contra o teto de 50 ms — e deixou em aberto
uma escolha binária, sem decidir:

1. Investir numa sexta task, ligando o skip index de `format_version` 5 ([ADR 0022](0022-postings-fts-skip-lists.md))
   ao hot path de `fts::search` via um algoritmo BlockMax-WAND (BMW) que pula blocos inteiros de
   postings sem decodificá-los; ou
2. Tornar o full-text opt-in (vector-only como default), aceitando 224,88 ms como limitação de
   escala documentada para o launch do M1.

O ADR 0017 já registrava a segunda opção como "não é decisão default desta fase" e listava o
modo `vector_only` opt-in em "Alternativas rejeitadas" como fallback apenas se o profiling
mostrasse causa não corrigível no prazo — não como plano A. Faltava, porém, o dado que fecha
essa avaliação: quanto o full-text **compra**, não só quanto **custa**.

Esse dado chegou com a medição de FT6 (`benches/src/lexical.rs`, mesmo ADR 0017, seção "O
benefício do full-text"): 100 queries lexicais determinísticas (identificadores de código,
flags de CLI, fragmentos de erro literal, hashes hex, ULIDs), ground truth por construção,
comparando `Store::recall` (híbrido) contra `Store::recall_vector` (vetor-puro) no mesmo
dataset materializado.

### Full-text lift medido (`benches/results/0.1.0-dev.json`, rodada oficial 2026-07-13)

| dataset | recall@10 híbrido | recall@10 vetor-puro | lift | p99 híbrido | p99 vetor-puro |
|---|---:|---:|---:|---:|---:|
| agent-mem-10k | 1,0000 | 0,9100 | **+0,09** | 22,14 ms | 18,63 ms |
| agent-mem-100k | 1,0000 | 0,8200 | **+0,18** | 139,38 ms | 32,45 ms |

O lift **dobra** de @10k para @100k, na direção oposta à hipótese que teria favorecido
vector-only default: um corpus maior aumenta a colisão vetorial entre literais parecidos
(quase-sinônimos no espaço de embedding), degradando o recall vetor-puro (0,9100 → 0,8200),
enquanto o híbrido segura 100% nos dois tamanhos — o BM25 continua encontrando o literal exato
independentemente do quanto o espaço vetorial fica populado ao redor dele. O custo do full-text
sobre essas mesmas queries lexicais (o mesmo gargalo já isolado no ADR 0017) segue presente e
cresce com o corpus (p99 híbrido 139,38 ms vs. 22,14 ms @10k), mas a tendência do lift é a
evidência que faltava: crescente, não decrescente, com a escala.

## Decisão

**Full-text continua como default, não opt-in. Investe-se na reescrita BlockMax-WAND** sobre o
skip index de `format_version` 5 para fechar o NFR de latência, em vez de tornar o full-text
uma escolha do usuário.

Razões:

- O lift medido cresce com o corpus (+0,09 → +0,18), não encolhe — a hipótese que justificaria
  vector-only default (custo sem benefício crescente) não se confirmou; o oposto se confirmou.
- O posicionamento do produto (`00-prd.md` §3, já citado no ADR 0017 "Alternativas rejeitadas")
  depende do full-text estar disponível por padrão — "híbrido de verdade" deixaria de ser um
  diferencial auditável se a maioria dos usuários rodasse com FTS desligado por causa da
  latência, quando o dado agora mostra que o próprio full-text é o que entrega recall correto em
  queries lexicais num corpus grande — exatamente o caso onde vector-only falha mais (0,8200,
  pior que a @10k).
- O caminho técnico para fechar o NFR já está com o pré-requisito pronto: o ADR 0022 entregou a
  **estrutura** (formato v5, lookup por bloco, equivalência auditada, fuzz) deliberadamente sem
  ligar o skip ao hot path (ADR 0022 §5, "Honestidade sobre onde o ganho entra") — a passada 1 de
  `fts::search` ainda materializa a lista inteira de cada termo; o BMW é o algoritmo que faltava
  para o skip index cortar trabalho de verdade, pulando um bloco inteiro quando seu
  `max_term_freq` prova que nenhuma entrada dele entra no top-k.

### Fila da fase BMW (substitui a "decisão pendente do founder" do ROADMAP)

1. **BMW1 — Reescrita da passada 1 de `fts::search` em BlockMax-WAND**, sobre o skip index fv5
   já existente ([ADR 0022](0022-postings-fts-skip-lists.md)): pular blocos cujo `max_term_freq`
   não pode entrar no top-k, sem decodificar suas entradas. Maior risco de equivalência da fase —
   muda a ordem de avaliação dos candidatos, exige provar (oráculo `search_profiled` + fuzz) que
   o resultado permanece byte-idêntico ao scan exaustivo em todos os regimes de empate.
2. **BMW2 — Medição @ 10k e @ 100k pelo harness oficial**, mesma metodologia do ADR 0017
   (`benches/run_all.sh --full`), decidindo se o NFR `recall p99 @ 100k < 50 ms` passa.
3. **BMW3 — Fechamento**: atualizar ADR 0017/0022/README/ROADMAP com o resultado, seja ele qual
   for.

### Critério de reversão (honesto, registrado antes do resultado)

Se o BMW **não** fechar o NFR (`recall p99 @ 100k < 50 ms`) **ou** quebrar a equivalência de
resultado (byte-idêntico ao scan exaustivo, provado pelo oráculo `search_profiled`) dentro do
esforço razoável de uma task própria, a opção vector-only default **volta à mesa** — não fica
descartada permanentemente por esta decisão. O lift medido justifica *tentar* o caminho que
preserva "híbrido por padrão"; não é uma garantia de que a reescrita vai fechar o teto de 50 ms.
Essa reavaliação, se necessária, é decisão do founder na task de fechamento (BMW3), com o
resultado medido em mãos — o mesmo padrão de honestidade que fechou a fase FT.

## Alternativas rejeitadas

- **Vector-only default (full-text opt-in) agora, sem esperar o BMW**: rejeitada com base no
  lift medido — cortaria exatamente o caso onde o full-text mais entrega valor (corpus grande,
  literais exatos), trocando um NFR de latência reprovado por uma regressão silenciosa de
  qualidade em queries lexicais que a maioria dos usuários de agente de IA gera (nomes de
  função, flags, hashes, IDs). Fica descartada como decisão *deste* ADR, não como impossível para
  sempre — ver "Critério de reversão" acima.
- **Aceitar 224,88 ms/255,12 ms como limitação de escala documentada, sem tentar o BMW**:
  rejeitada pelo mesmo motivo do ADR 0017 original — a estrutura (skip index fv5) já existe e foi
  construída deliberadamente para este próximo passo (ADR 0022 §5); não tentar o BMW deixaria
  trabalho já pago sem uso e sem resposta à pergunta que o profiling (FT1) e a estrutura (FT3
  parte 2) deixaram aberta.

## Consequências

- A fase BMW é uma sequência de risco crescente, mesmo padrão da fase FT: estrutura antes de
  algoritmo (já feito, ADR 0022), depois o algoritmo de scan (BMW1, o de maior risco de
  equivalência), só então a medição decide (BMW2/BMW3).
- Se o BMW não fechar o NFR, a decisão sobre vector-only default volta a ser aberta — este ADR
  não a fecha de forma irreversível, apenas resolve a ordem em que as opções são tentadas.
- `README.md` e `ROADMAP.md` passam a apontar para este ADR (não mais para "decisão pendente do
  founder") como a decisão vigente sobre o rumo pós-fase-FT.
