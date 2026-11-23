# Encontro pelos relays Nostr

Terceiro caminho para o pedido e a resposta da entrada, junto com a DHT e o encontro
direto (docs/encontro-direto.md). Vale a resposta que chegar primeiro.

Os relays Nostr são centenas de servidores públicos e gratuitos, mantidos por gente
diferente, que repassam mensagens assinadas por WebSocket na porta 443 (passa em quase
qualquer rede). Não são servidores nossos e não exigem conta: dá para trocar qualquer um
deles por outro. Usamos quatro (`nostr::RELAYS`), todos ao mesmo tempo.

## Como funciona

- Eventos efêmeros (tipo 25050): o relay entrega na hora a quem está inscrito e não
  guarda nada.
- O host, enquanto transmite, fica inscrito na marca da transmissão (`aj1-<marca>`, a
  mesma base da marca do encontro direto, tirada do código e da senha).
- O espectador se inscreve numa marca só desta tentativa e publica o pedido na marca da
  transmissão, com a própria marca de resposta dentro (tudo cifrado), de 12 em 12 s até
  ter a resposta.
- O host responde na marca de resposta (e também pela DHT e direto, como antes).
- Cada canal assina os eventos com uma chave descartável (só os relays exigem
  assinatura; quem confere o conteúdo é a cifra com a chave do código).

Medido com os relays de verdade: resposta em menos de 1 s (pela DHT, ~17 s no mesmo
teste).

## Segurança e privacidade

- O conteúdo vai cifrado com a chave do código: os relays veem só a marca e bytes
  embaralhados. Sem o código (e a senha, se houver), ninguém lê nem forja pedidos.
- Os relays veem o IP de quem se conecta a eles (como qualquer site) e que alguém usou
  aquela marca naquela hora.
- Desligado com `AJ_NO_NOSTR=1`.
