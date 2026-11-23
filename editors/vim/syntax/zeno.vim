" Colours for Zeno in Vim and Neovim.
if exists("b:current_syntax")
  finish
endif

syntax keyword zenoDeclare let var const function class enum interface
syntax keyword zenoModifier owned take strict unsafe private
syntax keyword zenoControl if else for in while break continue match case
syntax keyword zenoControl try except raise raises return import from as assert
" `test` is a name anywhere but at the start of a test: `test "what it checks" {`
syntax match zenoControl "^\s*\zstest\ze\s\+[\"']"
syntax keyword zenoWord and or not is
syntax keyword zenoConstant true false none
syntax keyword zenoSelf self
syntax keyword zenoType int float bool str list dict pointer
syntax keyword zenoType u8 u16 u32 u64 i8 i16 i32 f32

syntax match zenoTypeName "\<[A-Z][A-Za-z0-9_]*\>"
syntax match zenoCall "\<[a-z_][A-Za-z0-9_]*\ze("
syntax match zenoNumber "\<0[xX][0-9a-fA-F_]\+\>"
syntax match zenoNumber "\<0[bB][01_]\+\>"
syntax match zenoNumber "\<0[oO][0-7_]\+\>"
syntax match zenoNumber "\<[0-9][0-9_]*\(\.[0-9][0-9_]*\)\=\([eE][+-]\=[0-9]\+\)\="

syntax match zenoEscape contained "\\[ntr0\\\"']"
syntax region zenoPlaceholder contained matchgroup=zenoBrace start="{" end="}" oneline contains=zenoNumber,zenoCall,zenoWord,zenoConstant
syntax region zenoText start=+"+ skip=+\\.+ end=+"+ oneline contains=zenoEscape
syntax region zenoText start=+'+ skip=+\\.+ end=+'+ oneline contains=zenoEscape
syntax region zenoText start=+\<f"+ skip=+\\.+ end=+"+ oneline contains=zenoEscape,zenoPlaceholder
syntax region zenoText start=+\<f'+ skip=+\\.+ end=+'+ oneline contains=zenoEscape,zenoPlaceholder
syntax match zenoComment "#.*$" contains=@Spell

" `let lib = inline c {` to the `}` under the first letter of that line:
" code of another language, which is not coloured as Zeno. `inline` is a
" name anywhere else.
syntax region zenoInline matchgroup=zenoInlineEdge start="^\z(\s*\).*\<inline\s\+\h\w*\s*{\s*$" end="^\z1}\s*$" keepend

highlight default link zenoDeclare Keyword
highlight default link zenoInline PreProc
highlight default link zenoInlineEdge Keyword
highlight default link zenoModifier StorageClass
highlight default link zenoControl Statement
highlight default link zenoWord Operator
highlight default link zenoConstant Constant
highlight default link zenoSelf Identifier
highlight default link zenoType Type
highlight default link zenoTypeName Type
highlight default link zenoCall Function
highlight default link zenoNumber Number
highlight default link zenoEscape SpecialChar
highlight default link zenoBrace Special
highlight default link zenoText String
highlight default link zenoComment Comment

let b:current_syntax = "zeno"
