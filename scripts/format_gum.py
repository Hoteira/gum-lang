import os
import glob

def format_file(filepath):
    with open(filepath, 'r') as f:
        lines = f.readlines()
        
    out = []
    in_contract = False
    
    for line in lines:
        if line.startswith('contract ') or line.startswith('extern class '):
            in_contract = True
            out.append(line)
        elif in_contract and not line.startswith('contract ') and not line.startswith('extern class ') and not line.startswith('use ') and not line.startswith('enum '):
            if line.strip() == '':
                out.append('\n')
            elif line.startswith('    '): # already indented (was a class field or already indented function body)
                out.append('    ' + line.lstrip(' '))
            else:
                out.append('    ' + line)
        else:
            in_contract = False
            out.append(line)
            
    with open(filepath, 'w') as f:
        f.writelines(out)

for f in ['token.gum', 'amm.gum', 'bench/erc20.gum', 'bench/erc721.gum', 'bench/vault.gum']:
    if os.path.exists(f):
        format_file(f)

