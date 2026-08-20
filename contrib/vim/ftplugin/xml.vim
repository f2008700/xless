" Default mapping for filetype=xml buffers — opens the current file in
" xless, focused at the cursor line. See README.md's Vim integration
" section, §3.
"
" <leader>xx (not gx/gX, which several XML/HTML plugins and netrw already
" claim) so this doesn't fight other ftplugins for the same filetype.
if !exists('g:xless_no_default_mapping') && !hasmapto('<Plug>XlessOpen')
  nnoremap <buffer> <silent> <leader>xx :Xless<CR>
endif
