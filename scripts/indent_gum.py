import os
import glob

def process_file(filepath):
    with open(filepath, 'r') as f:
        lines = f.readlines()

    out = []
    in_contract = False
    for line in lines:
        if line.startswith('contract '):
            in_contract = True
            out.append(line)
        elif in_contract and (line.startswith('fn ') or line.startswith('export fn ')):
            out.append('    ' + line)
        elif in_contract and (line.startswith('    ') or line.strip() == '' or line.startswith('}')):
            if line.startswith('    '):
                out.append('    ' + line)
            elif line.strip() == '':
                out.append(line)
            else:
                out.append(line) # ignore
        elif in_contract and not line.startswith('contract '):
            # Probably some other top level thing like a variable definition inside the contract
            out.append(line)
        else:
            out.append(line)
            
    with open(filepath, 'w') as f:
        f.writelines(out)

for file in glob.glob('*.gum') + glob.glob('bench/*.gum'):
    process_file(file)

