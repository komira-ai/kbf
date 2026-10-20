Attack corpus for the Markdown renderer (readme mode). Each case starts with a line "%%% <name>"; the test
renders every case on its own and the whole file as one document, and checks that only renderer markup and safe
link targets come out.

%%% script tag
<script>alert(1)</script>

%%% script split across lines
<scr
ipt>alert(1)</script>

%%% img onerror
<img src=x onerror=alert(1)>

%%% svg onload
<svg/onload=alert(1)>

%%% iframe
<iframe src="https://mopm.dev"></iframe>

%%% style attribute via html
<p style="color:red">x</p>

%%% javascript scheme
[x](javascript:alert(1))

%%% mixed-case javascript scheme
[x](JaVaScRiPt:alert(1))

%%% entity-encoded scheme
[x](java&#115;cript:alert(1))

%%% entity-encoded colon
[x](javascript&#58;alert(1))

%%% percent-encoded scheme
[x](%6Aavascript:alert(1))

%%% tab inside the scheme (angle destination)
[x](<java	script:alert(1)>)

%%% leading space in an angle destination
[x](< javascript:alert(1)>)

%%% data url
[x](data:text/html;base64,PHNjcmlwdD5hbGVydCgxKTwvc2NyaXB0Pg==)

%%% vbscript
[x](vbscript:msgbox(1))

%%% file scheme
[x](file:///etc/passwd)

%%% protocol-relative
[x](//evil/x)

%%% backslash protocol-relative
[x](/\\evil/x)

%%% javascript autolink
<javascript:alert(1)>

%%% data image
![x](data:image/svg+xml,<svg onload=alert(1)>)

%%% javascript image
![x](javascript:alert(1))

%%% link in a table cell
| a | b |
|---|---|
| [x](javascript:alert(1)) | <img src=x onerror=alert(1)> |

%%% link in a heading
# [x](javascript:alert(1))

%%% link in a quote in a list
- > [x](javascript:alert(1))

%%% quote in attribute position
[x](https://mopm.dev/"onmouseover="alert(1))

%%% angle brackets in a destination
[x](<https://mopm.dev/<script>>)

%%% title with markup
[x](https://mopm.dev "<script>alert(1)</script>")

%%% html comment
<!-- <script>alert(1)</script> -->

%%% cdata and processing instruction
<![CDATA[x]]> <?php echo 1 ?>

%%% nested emphasis and brackets
*[**[x](javascript:alert(1))**]*  [[[[a]]]](https://mopm.dev) ![![x](y)](z)

%%% code span holding html
`<script>alert(1)</script>`

%%% fenced code holding html
```html
<script>alert(1)</script>
```

%%% fragment trying to break out
[x](#"><script>alert(1)</script>)

%%% heading id clobbering
# __proto__
# constructor
# location

%%% mailto with header injection
[x](mailto:a@mopm.dev?bcc=b@mopm.dev%0d%0aSubject:hi)

%%% image inside a link (badge) must not nest anchors
[![CI](https://mopm.dev/ci.svg)](https://mopm.dev/actions) [![](https://mopm.dev/b.svg)](https://mopm.dev)

%%% image inside emphasis inside a link
[a **![b](https://mopm.dev/b.svg)** c](https://mopm.dev/a)

%%% empty link text
[](https://mopm.dev) [ ](https://mopm.dev/x)
