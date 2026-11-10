def quote(zone, grams, express=False):
    if type(zone) is not str or zone not in ('local', 'remote'):
        raise ValueError('invalid zone')
    if type(grams) is not int or not 1 <= grams <= 20000:
        raise ValueError('invalid weight')
    if type(express) is not bool:
        raise ValueError('invalid express flag')
    base = 400 if zone == 'local' else 900
    step = 75 if zone == 'local' else 150
    price = base + max(0, (grams - 1000 + 499) // 500) * step
    return price + (600 if express else 0)


def tracking_label(order_id):
    if type(order_id) is not str or not order_id or not order_id.isascii():
        raise ValueError('invalid order ID')
    return 'TRACK:' + order_id.upper()
