# https://github.com/n-WN/share_these/blob/main/test/test.sh
for i in {1..10}; do
    offset=$(( (i-1) * 1073741824 ))  # 10GB/10 = 1GB per client
    curl -s -o /dev/null --limit-rate 100M --range $offset-$(( offset + 1073741823 )) \
         "http://localhost:8000/10gbfile" &
done
wait