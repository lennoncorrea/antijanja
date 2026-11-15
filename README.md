# AntiJanja

Compartilhamento de tela **100% P2P**, sem servidor central e sem conta.

**Quem transmite envia um código. Quem assiste cola o código. Pronto.**

## Como funciona

| Parte | Como |
|---|---|
| Vídeo e áudio | WebRTC nativo (webrtc-rs), criptografado com DTLS-SRTP, direto entre os computadores |
| Encontro dos peers | DHT Mainline do BitTorrent (BEP44), usada como "caixa postal" criptografada — em Rust |
| Descoberta de IP público | STUN público (só informa o IP; nenhum dado de vídeo passa por ele) |

O código `AJ2.…` carrega 32 bytes aleatórios. Dele o app deriva as chaves de assinatura e a chave
de criptografia (ChaCha20-Poly1305) das mensagens trocadas na DHT. Os nós da DHT só veem dados
criptografados; sem o código ninguém lê ou forja mensagens.

1. O host gera o código e fica consultando a DHT.
2. O espectador cola o código, cria uma oferta WebRTC e a publica na DHT.
3. O host encontra a oferta sozinho, responde e a conexão P2P direta é estabelecida.

O encontro leva em média ~15–20 s (latência da DHT). Depois disso a DHT não participa mais.

**Limitações:** se os dois lados estiverem atrás de NAT simétrico / CGNAT, a conexão direta pode
falhar — não há relay, por design. Qualquer pessoa com o código pode entrar; use *gerar novo código*
para invalidar o anterior (quem já está assistindo continua).

## Desenvolvimento

Tudo em Rust, sem navegador embutido: a interface é feita em [Slint](https://slint.dev)
(arquivos em `ui/`) e o motor (captura, H.264, WebRTC, DHT, áudio, player) fica em `src/`.

Requisitos: Rust (stable). Para gerar o instalador: NSIS e Node.js (só para assinar a atualização).

```powershell
cargo run                        # roda o app (modo desenvolvimento)
cargo test                       # testes unitários
cargo test -- --ignored --nocapture  # testes contra a DHT real (precisa de internet)

.\scripts\release.ps1            # build + instalador assinado + latest.json
.\scripts\release.ps1 -Publish   # e publica a release no GitHub (atualiza quem já tem o app)
```

Os builds rodam com poucas tarefas e prioridade baixa (`CARGO_BUILD_JOBS=4`) para não travar o PC.

Fontes: [Inter](https://rsms.me/inter/) e [JetBrains Mono](https://www.jetbrains.com/lp/mono/), ambas OFL (`assets/fonts`).
