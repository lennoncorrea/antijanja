# Fofoca entre os apps: "Ao vivo agora" na hora

Hoje a lista "Ao vivo agora" fica num mural na DHT do BitTorrent, e quem está no Início
relê o mural de tempos em tempos (cada leitura são 48 consultas à DHT). A DHT só responde
quando alguém pergunta: não existe "avise quando mudar". Uma live nova aparecia entre uns
5 e 25 s depois de começar.

A fofoca é uma segunda rede, só entre os apps AntiJanja abertos. Quem começa (ou encerra,
ou muda) uma live conta para os vizinhos, que contam para os vizinhos deles; em menos de
um segundo todo mundo conectado sabe. Ninguém fica consultando: cada app mantém algumas
conexões abertas e recebe os avisos.

O mural na DHT continua sendo a fonte de reserva: com vizinhos na fofoca, a lista só é
relida pela DHT de vez em quando (para pegar quem ficou de fora da fofoca); sem vizinhos,
volta ao ritmo de antes.

## Como os apps se encontram

1. **DHT, por tópico.** O app anuncia o próprio endereço no tópico do AntiJanja
   (`announce_peer`, o mesmo recurso que o BitTorrent usa para "quem tem este arquivo") e
   busca quem mais está lá (`get_peers`). A busca só acontece ao abrir e de tempos em
   tempos; não é para receber os avisos.
2. **Endereço público.** O app descobre o próprio endereço público com um STUN (o mesmo
   servidor público que o WebRTC já usa) e confirma com o que os vizinhos dizem ver.
3. **Atravessar o roteador.** Os dois lados mandam pacotes um para o outro; quando os dois
   mandaram, os roteadores deixam passar. Quem já está na rede apresenta vizinhos entre si
   ("fulano vai te chamar, chame ele também") para os dois mandarem ao mesmo tempo.
4. **Mesma rede ou mesmo PC.** O endereço público não funciona entre dois apps atrás do
   mesmo roteador; um aviso por multicast na rede local acha esses apps.

Quem está atrás de um roteador que muda a porta a cada destino (NAT simétrico, comum em
redes de celular) às vezes não consegue vizinho nenhum: continua com a lista pela DHT.

## Mensagens

UDP, um pacote por mensagem (no máximo 1400 bytes): `AJG1` + JSON.

- `hi` / `hey`: quero conversar / aceito (com o endereço em que vejo você).
- `ping` / `pong`: mantém a passagem pelo roteador aberta (a cada 15 s); vizinho mudo por
  50 s sai da lista.
- `peers` + `meet`: troca de vizinhos e apresentação para atravessar o roteador.
- `live`: um anúncio do mural (a mesma entrada assinada da DHT), com o número de saltos.
- `sync`: quem acabou de chegar pede as lives atuais.

## Segurança

- Cada anúncio é a entrada do mural, assinada pela chave do host do código anunciado:
  ninguém anuncia, encerra ou muda uma live que não é sua. Entrada com assinatura errada,
  velha (mais de 3 min) ou repetida é descartada e não é repassada.
- Limite de mensagens por vizinho e de saltos por anúncio.
- `sync` e repasse só para vizinhos que provaram o endereço (responderam a um `hi` ou a um
  `ping` nosso): um pedido com endereço falsificado não faz o app mandar a lista para outro.
- Os vizinhos veem o IP uns dos outros (como na DHT e no WebRTC).

O que não dá para impedir sem servidor: alguém criar muitas chaves e anunciar lives falsas
(o mesmo vale para o mural na DHT hoje).
