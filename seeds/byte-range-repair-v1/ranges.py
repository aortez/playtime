import json
import sys

def resolve_ranges(header, size):
    if not header:
        return {"status":200,"ranges":[[0,size-1]]}
    intervals=[]
    for member in header.split('=')[1].split(','):
        start,end=member.split('-')
        start=int(start or 0)
        end=int(end or size-1)
        intervals.append([start,min(end,size-1)])
    return {"status":206,"ranges":intervals}

if __name__=='__main__':
    print(json.dumps(resolve_ranges(sys.argv[2],int(sys.argv[1]))))
