import json
import sys

def plan(tasks):
    done = set()
    result = []
    while len(done) < len(tasks):
        wave = []
        for task in tasks:
            if task['id'] not in done and all(d in done for d in task['deps']):
                wave.append(task['id'])
                done.add(task['id'])
        if not wave:
            raise ValueError('cycle')
        result.append(wave)
    return result

if __name__ == '__main__':
    with open(sys.argv[1]) as source:
        result = plan(json.load(source))
    with open(sys.argv[2], 'w') as target:
        json.dump(result, target)
