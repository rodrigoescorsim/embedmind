# RELEASING — publicação no crates.io

Como publicar os crates do EmbedMind no [crates.io](https://crates.io). Cobre a
**ordem obrigatória**, os passos `[MANUAL — founder]`, e um limite do crates.io
que afeta o `embedmind-core` (modelo ONNX embarcado).

> **`cargo publish` real é ato do founder** (guardrail 7 em
> [04-agents.md](04-agents.md); item 1.6 marcado `[MANUAL — founder]` no
> [03-tasks.md](03-tasks.md)). Nenhum agente/CI publica sozinho. Este documento
> prepara tudo até a borda do `cargo publish`; o comando final é digitado por uma
> pessoa, no dia do launch.

## Os três crates

| Crate | Publica como | Papel |
|---|---|---|
| `embedmind-core` | `embedmind-core` | a engine (o ativo) |
| `embedmind-mcp` | `embedmind-mcp` | servidor MCP (casca) |
| `embedmind-cli` | **`embedmind`** | CLI + `embedmind serve` (casca) |

Nomes confirmados **disponíveis** no crates.io em jul/2026 (`embedmind`,
`embedmind-core`, `embedmind-mcp` — nenhum existe ainda no índice).

Metadados de publicação (`description`, `repository`, `homepage`, `keywords`,
`categories`, `readme`, `license = "MIT"`) já estão em cada `Cargo.toml`;
`repository`/`homepage` são herdados de `[workspace.package]`. Cada crate tem seu
próprio `README.md` (o `cargo publish` só empacota arquivos dentro do diretório
do crate — o README da raiz não é alcançável a partir de `crates/*/`).

## Ordem obrigatória: core → mcp → cli

**A ordem não é opcional.** Um crate só pode ser publicado depois que todas as
suas dependências de workspace já estão no índice do crates.io. As dependências
internas carregam `path` **e** `version` em `[workspace.dependencies]`; ao
publicar, o cargo remove o `path` e resolve pela `version` **no índice** — que só
existe após o upstream estar publicado.

```
1. embedmind-core   (sem deps internas)
2. embedmind-mcp    (depende de embedmind-core)
3. embedmind        (depende de embedmind-core e embedmind-mcp)
```

Consequência prática para verificação: `cargo publish --dry-run -p embedmind-mcp`
(e `-p embedmind`) **falha** enquanto o `embedmind-core` não estiver publicado,
com `no matching package named 'embedmind-core' found` — isso é esperado, não é
um defeito de manifesto. Para validar os três localmente **antes** de qualquer
publicação, use `cargo package --workspace` (resolve as deps internas em
conjunto):

```bash
cargo package --workspace          # empacota + compila os 3, na ordem
```

Saída esperada (jul/2026): os 3 pacotes empacotam e compilam sem erro.

## Verificação pré-publicação

```bash
# 1. Suíte verde em todas as plataformas de primeira classe (CI faz isso).
cargo test --workspace

# 2. Lints limpos (pré-condição de commit do projeto).
cargo clippy --workspace --all-targets
cargo fmt --all --check

# 3. Publish-readiness dos 3 crates, resolvendo deps internas em conjunto.
cargo package --workspace

# 4. Dry-run do core isolado (o único que resolve sem upstream publicado).
cargo publish --dry-run -p embedmind-core
```

`cargo publish --dry-run` para `embedmind-mcp` e `embedmind` só fica limpo
**depois** que o `embedmind-core` (e, para o CLI, o `embedmind-mcp`) estiverem
realmente no crates.io — ver seção da ordem acima.

## Limite de tamanho do crates.io (`embedmind-core`) — resolvido por `build.rs`

O crates.io impõe um teto de **10 MiB por crate** (pacote comprimido). O
modelo ONNX quantizado (`model_quantized.onnx`, ~22 MB não-comprimido) sozinho
estoura esse teto: com ele embarcado no pacote fonte, o `embedmind-core`
empacotava em **~16 MiB comprimido** e o `cargo publish` real seria **rejeitado**
(`crate too large`). O `--dry-run` não pegava isso, pois o teto é aplicado **no
servidor**, no upload — o dry-run não sobe nada.

**Resolução (implementada, sem passo manual no crates.io):** o modelo ONNX **não
vai** dentro do pacote fonte publicado; é **re-baixado e verificado por checksum
em tempo de build** por `crates/embedmind-core/build.rs`, no mesmo espírito que o
`ort` já usa com `download-binaries` para a lib do ONNX Runtime. O
**comportamento observável não muda**: o binário final continua embarcando o
modelo via `include_bytes!` (ADR [0004](adr/0004-modelo-de-embedding-embarcado.md)),
então `cargo install embedmind` roda sem rede em runtime e "nada sai da máquina".
O que muda é apenas o que vai **dentro** do `.crate` publicado.

Como o `build.rs` resolve o modelo (nunca embarca bytes não-verificados):

1. **Checkout de dev / CI** — o asset está na árvore
   (`assets/all-MiniLM-L6-v2/onnx/model_quantized.onnx`). Usa direto, sem rede.
2. **Build a partir do crate publicado** — o asset foi `exclude`do do pacote,
   então baixa-o **uma vez** de um export content-addressed do Hugging Face
   (`Xenova/all-MiniLM-L6-v2`) para um cache local (`$CARGO_HOME/embedmind/models/`,
   com fallback para `OUT_DIR`), keyed pelo checksum.

Nos dois caminhos os bytes são conferidos contra um **SHA-256 fixado**
(`afdb6f1a…`, 22_972_370 bytes) antes de qualquer `include_bytes!` — um download
truncado, um mirror trocado ou um cache adulterado falham o build em vez de
serem linkados. O artefato do Hugging Face foi verificado **byte a byte**
idêntico ao asset in-tree, então checkout e download produzem o **mesmo binário**.
O tokenizer/vocab (bem abaixo de 1 MB) continuam embarcados no crate como antes.

Escape hatch offline/air-gapped e vendoring: apontar
`EMBEDMIND_MODEL_ONNX=/caminho/model_quantized.onnx` para uma cópia pré-baixada
(ainda checksum-verificada — sem bypass de integridade).

Tamanho do pacote depois da mudança:

```
Packaging embedmind-core ...
Packaged 36 files, 1.4MiB (452KiB compressed)      # era 16.2 MiB comprimido
```

O `.crate` comprimido (**~452 KiB**) é o que o crates.io compara com o teto de
10 MiB — folga de sobra, sem pedido manual de aumento de limite.

Verificação de que o build a partir do pacote publicado ainda funciona (compila
o `.crate` extraído num diretório isolado, onde o modelo não existe → força o
download + checksum do `build.rs`):

```bash
cargo package -p embedmind-core     # empacota E compila a partir do extraído; deve terminar sem erro
```

Alternativas **rejeitadas** (contrariam decisões vigentes):

- **Pedir aumento de limite ao crates.io e manter o modelo embarcado no pacote**
  — funciona, mas depende de um passo manual/aprovação externa a cada bump de
  formato; a estratégia de `build.rs` remove essa dependência sem custo de UX.
- **Fetch do modelo em *runtime*** — reintroduz rede no núcleo em runtime;
  contraria ADR 0004 e "nada sai da máquina" / air-gap. (O download aqui é em
  *build time* e o binário resultante é auto-contido — a promessa de runtime
  fica intacta.)
- **Modelo menor só para caber no teto** — decisão de qualidade (recall/pt-BR),
  fora de escopo aqui; já rastreada como aberta (DESIGN §12, ADR 0004/0010).

> Nota: o teto de **40 MB** do artefato de release (ADR
> [0010](adr/0010-teto-de-tamanho-governa-artefato-comprimido.md),
> `release.yml`) é um NFR **diferente** — governa o binário comprimido que o
> usuário baixa das Releases (modelo embarcado, ~inalterado por esta mudança),
> não o pacote do crates.io.

## Smoke test de instalação (binário pré-compilado)

Antes de anunciar uma Release, valide o binário **como o usuário o recebe**: numa
pasta limpa, sem depender do checkout, exercitando o fluxo do quickstart
(`--version` → `remember` → `recall` → `stats`) sobre um `.mind` descartável. O
script [`scripts/smoke_install.sh`](../scripts/smoke_install.sh) faz isso e sai
com código ≠ 0 na primeira asserção que falhar (serve de porta de CI e de
checklist de launch).

```bash
# 1. Contra o binário baixado das Releases (o caso que importa no launch):
tar xzf embedmind-<versão>-<alvo>.tar.gz     # ou unzip no Windows
EMBEDMIND_BIN=./embedmind ./scripts/smoke_install.sh
#   Windows (bash/Git Bash):  EMBEDMIND_BIN=./embedmind.exe ./scripts/smoke_install.sh

# 2. Contra o que estiver no PATH (ex. após `cargo install embedmind`):
./scripts/smoke_install.sh

# 3. Sem binário instalado (checkout de código): faz fallback para `cargo run`.
./scripts/smoke_install.sh
```

O que o smoke test comprova, além de "roda":

- `--version` imprime `embedmind <semver>` (nome + versão bem-formados);
- `remember --global` devolve um ULID de 26 chars marcado `(global)` — grava e
  reconhece a memória;
- `recall` com uma consulta **semanticamente próxima mas de texto diferente**
  reencontra aquele id e ecoa o conteúdo — prova embedding + busca vetorial
  reais, não casamento de substring;
- `stats` reporta exatamente 1 memória viva **e** um modelo de embedding gravado
  no arquivo (`all-MiniLM-L6-v2-int8`, não `none (KV-only)`) — se o modelo ONNX
  não tivesse sido linkado no binário, esta asserção pegaria.

Tudo acontece num diretório temporário apagado na saída (`--file` próprio), então
o `~/.embedmind/memory.mind` real do founder nunca é tocado.

> `[MANUAL — founder]` complementar (fora deste script): abrir o `.mind` gerado
> num **segundo agente** (ex. Cursor) e confirmar `recall` do outro lado — a
> prova de integração ponta a ponta que precisa de um cliente MCP externo.

Complementar, [`scripts/smoke_gif_script.sh`](../scripts/smoke_gif_script.sh) valida
literalmente o roteiro do [GIF de demo](launch/gif-script.md) (30s, 5 beats:
`remember` ×2 → `recall` → `stats`, arquivo padrão `~/.embedmind/memory.mind`, fora de
um repo git) contra o binário real, comando a comando — inclusive a alegação de que o
`recall` do beat 3 vence por semântica (a memória top-ranked é conferida, não só
presente no output). Roda com a mesma variável `EMBEDMIND_BIN`.

## Portabilidade do `.mind` entre plataformas (verificação cross-platform)

Garantia de formato **G3** ([docs/FORMAT.md](FORMAT.md) §1): o arquivo é
byte-idêntico entre plataformas porque todo inteiro multi-byte é gravado em
**little-endian fixo** — nunca na ordem nativa do host. Um `.mind` escrito no
Windows do founder abre idêntico em Linux/macOS, e vice-versa.

Coberto automaticamente por
[`crates/embedmind-core/tests/portability.rs`](../crates/embedmind-core/tests/portability.rs)
(roda em `cargo test --workspace`, portanto em toda a matriz de CI):

- `header_is_fixed_little_endian_per_format_spec` — grava um `.mind` real e
  afirma que os campos do header (page 0) estão nos offsets de FORMAT §4 **em
  little-endian explícito** (ex. `format_version = 1` são os bytes
  `01 00 00 00`, não `00 00 00 01`). Num host big-endian, um bug de
  ordem-nativa faria estes bytes divergirem — é o que a asserção proíbe.
- `written_store_reopens_with_identical_content` — o round-trip que a
  portabilidade protege: escrever → reabrir → conteúdo idêntico.

Verificação manual entre duas máquinas de arquiteturas diferentes (quando quiser
a prova ponta a ponta, além do teste unitário):

```bash
# Máquina A (ex. Windows x86-64):
embedmind --file portable.mind remember "cross-platform check" --global
sha256sum portable.mind          # anote o hash (certutil -hashfile no cmd puro)

# Copie portable.mind para a Máquina B (ex. Linux arm64) e lá:
sha256sum portable.mind          # DEVE bater com o hash da Máquina A
embedmind --file portable.mind recall "cross-platform" --all   # reencontra a memória
embedmind --file portable.mind stats                            # mesmas contagens
```

Hashes iguais confirmam G3 na prática; o `recall`/`stats` confirmam que o
arquivo idêntico também é semanticamente legível do outro lado. (Cada comando
que escreve — como o `remember` acima — faz checkpoint e **remove** o WAL
sidecar `portable.mind-wal` ao fechar o store, então após o `remember` resta só
o arquivo principal: copie apenas `portable.mind`. Um `portable.mind-wal`
residual só apareceria após um crash e seria reincorporado na próxima abertura.)

## Passos de publicação (dia do launch — `[MANUAL — founder]`)

Pré-requisitos: `cargo login` feito; versão de release já definida em
`[workspace.package]` (`0.1.0` — os 3 crates Rust, `bindings/python/Cargo.toml`
e `bindings/python/pyproject.toml` já foram bumped em conjunto, ver
CHANGELOG.md `[0.1.0]`). O limite de 10 MiB do crates.io **não** é bloqueio: o
`embedmind-core` empacota em ~452 KiB (o modelo ONNX é baixado+verificado por
`build.rs`, não vai no pacote — ver seção acima). O upload do core precisa de
rede na máquina do publicador *e* de rede em quem for buildar a partir do crate
publicado (para o download do modelo em build time).

```bash
cargo publish -p embedmind-core     # 1 — esperar aparecer no índice
cargo publish -p embedmind-mcp      # 2
cargo publish -p embedmind          # 3 (crate publica como "embedmind")
```

Entre passos, aguardar o crate anterior ficar disponível no índice (segundos a
poucos minutos). Cada versão publicada é **imutável** — não dá para republicar a
mesma versão; só é possível `yank`.

### Tag + GitHub Release (`[MANUAL — founder]`)

Depois dos 3 `cargo publish` (ou em paralelo, são independentes — a tag não
depende do índice do crates.io):

```bash
git tag -a v0.1.0 -m "v0.1.0"
git push origin v0.1.0        # dispara release.yml (build dos binários + wheels)
```

`release.yml` builda os binários pré-compilados (ADR 0010, teto de 40 MB
comprimido) para as plataformas de primeira classe e sobe como assets da
GitHub Release da tag `v0.1.0`. Conferir manualmente antes de publicar a
Release como pública:

- Assets presentes para todas as plataformas de primeira classe (Windows/
  Linux/macOS conforme `release.yml`), cada um dentro do teto de 40 MB.
- `scripts/smoke_install.sh` (ver seção acima) já rodado contra pelo menos um
  binário baixado da Release, não só localmente.
- Notas da Release: colar o corpo do CHANGELOG.md `[0.1.0]`.

### Wheels Python no PyPI (`[MANUAL — founder]`)

Bindings (`bindings/python/`, roadmap 2.5) publicam separado do crates.io,
via `maturin` — não fazem parte da ordem core→mcp→cli acima.

```bash
cd bindings/python
maturin build --release           # smoke test local do wheel antes do upload
maturin publish                   # sobe para pypi.org — requer conta/token PyPI configurado
```

`pyproject.toml` já está em `0.1.0` (PEP 440; sem sufixo `.dev0`). Wheel
builda contra o `embedmind-core` local por `path` (bindings ficam fora do
workspace principal — ver nota em `bindings/python/Cargo.toml`), então não
depende do crate já estar publicado no crates.io.
